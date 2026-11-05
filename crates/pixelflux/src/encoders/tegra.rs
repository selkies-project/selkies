//! Tegra hardware video encoding through the vendor V4L2 encoder.
//!
//! Tegra ships no `libnvidia-encode`, so the NVENC backend cannot open it, it publishes no render
//! node driver for the VA-API one to probe, and its encoder is not a plain V4L2 M2M node either:
//! the node is `/dev/nvhost-msenc` on JetPack 4 and `/dev/v4l2-nvenc` on JetPack 6, and that
//! JetPack 6 node is a `/dev/null` placeholder whose `open` the vendor `libnvv4l2.so` intercepts.
//! The library is therefore loaded at runtime, the way `nvenc.rs` loads `libcuda` and
//! `vaapi` loads `libva`, and the encoder is driven with ordinary V4L2 ioctls. Nothing is added
//! to the build.
//!
//! One host frame becomes one access unit like this:
//!
//! 1. `Raw2NvBuffer` copies the packed frame into a pitch-linear surface. That copy is the floor
//!    on Tegra: there is no zero-copy X11 capture to import instead.
//! 2. `NvBufferTransform` converts the surface to block-linear NV12 on the VIC block, which costs
//!    the CPU nothing.
//! 3. The NV12 surface is queued on the output plane as a dmabuf and the access unit comes back on
//!    the capture plane, which is mapped through the fd `VIDIOC_EXPBUF` hands back: mapping the
//!    encoder fd itself answers ENODEV, and the vendor's own NvBuffer does the same thing.
//!
//! The encoder returns a unit one or two frames after the frame that produced it, so the frame
//! number travels in the buffer timestamp and the wire header is written from what comes back.
//!
//! The ioctl numbers and structure layouts were read off a target's headers rather than written
//! from memory, and `abi_matches` checks the sizes those numbers encode.

use std::collections::VecDeque;
use std::ffi::{CString, c_char, c_int, c_uint, c_void};
use std::mem::size_of;
use std::ptr;
use std::sync::OnceLock;

use libloading::{Library, Symbol};

use super::codec::{
    Codec, FRAME_KEY, VIDEO_HEADER_LEN, av1_is_key, frame_type_from_key, h264_dpb_frames,
    h264_frame_type, h265_dpb_frames, h265_frame_type, push_video_header,
};
use super::reference::{Reference, ReferenceWindow};
use crate::CaptureSettings;

const V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE: u32 = 9;
const V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE: u32 = 10;
const V4L2_MEMORY_MMAP: u32 = 1;
const V4L2_MEMORY_DMABUF: u32 = 4;
const V4L2_PIX_FMT_NV12M: u32 = 0x3231_4d4e;
const V4L2_PIX_FMT_H264: u32 = 0x3436_3248;
/// The vendor library spells HEVC `H265`, not the `HEVC` of a mainline V4L2 encoder.
const V4L2_PIX_FMT_H265: u32 = 0x3536_3248;
const V4L2_PIX_FMT_AV1: u32 = 0x3031_5641;
/// The driver copies this buffer's timestamp to the access unit it produces.
const V4L2_BUF_FLAG_TIMESTAMP_COPY: u32 = 0x4000;

const VIDIOC_S_FMT: u64 = 0xc0d0_5605;
const VIDIOC_REQBUFS: u64 = 0xc014_5608;
const VIDIOC_QUERYBUF: u64 = 0xc058_5609;
const VIDIOC_QBUF: u64 = 0xc058_560f;
const VIDIOC_EXPBUF: u64 = 0xc040_5610;
const VIDIOC_DQBUF: u64 = 0xc058_5611;
const VIDIOC_STREAMON: u64 = 0x4004_5612;
const VIDIOC_STREAMOFF: u64 = 0x4004_5613;
const VIDIOC_S_PARM: u64 = 0xc0cc_5616;
const VIDIOC_S_EXT_CTRLS: u64 = 0xc020_5648;

const CID_BITRATE: u32 = 0x0099_09cf;
const CID_BITRATE_MODE: u32 = 0x0099_09ce;
const CID_H264_PROFILE: u32 = 0x0099_0a6b;
const CID_H265_PROFILE: u32 = 0x0099_0b01;
const CID_IDR_INTERVAL: u32 = 0x0099_0b02;
/// `V4L2_CID_MPEG_VIDEO_GOP_SIZE`: the encoder's own intra interval, which inserts a key frame
/// on its own whatever the IDR interval says.
const CID_GOP_SIZE: u32 = 0x0099_09cb;
const CID_VBV_SIZE: u32 = 0x0099_0b13;
const CID_INSERT_SPS_PPS_AT_IDR: u32 = 0x0099_0b17;
/// AV1 carries no parameter sets; its sequence header rides the frame under its own control.
const CID_AV1_HEADERS_WITH_FRAME: u32 = 0x0099_0b39;
const CID_HW_PRESET: u32 = 0x0099_0b1c;
const CID_INSERT_VUI: u32 = 0x0099_0b22;
const CID_MAX_PERFORMANCE: u32 = 0x0099_0b2a;
const CID_POC_TYPE: u32 = 0x0099_0b33;
const CID_FORCE_IDR_FRAME: u32 = 0x0099_0b37;
/// External reference-picture-set control (`V4L2_CID_MPEG_BASE` + 532, 541, and 542): the
/// session names, with each frame, which pictures the encoder may predict from.
const CID_NUM_REFERENCE_FRAMES: u32 = 0x0099_0b14;
const CID_INPUT_METADATA: u32 = 0x0099_0b1d;
const CID_EXTERNAL_RPS: u32 = 0x0099_0b1e;
/// `V4L2_ENC_INPUT_RPS_PARAM_FLAG`.
const INPUT_METADATA_RPS: u32 = 1 << 2;
/// `V4L2_MAX_REF_FRAMES`, the length of the reference list a frame carries.
const RPS_LIST_LEN: usize = 8;
/// The widest picture order count LSB H.265 allows: H.265 names its references by POC, and the
/// encoder's own default wraps within the few frames a loss spans far sooner.
const H265_POC_LSB_BITS: u32 = 16;

/// Every encoder control belongs to this class; the driver ignores a request that
/// arrives without it, silently, which is how a profile and a VUI went missing.
const V4L2_CTRL_CLASS_MPEG: u32 = 0x0099_0000;
const BITRATE_MODE_CBR: i32 = 1;
const H264_PROFILE_MAIN: i32 = 2;
const H265_PROFILE_MAIN: i32 = 0;
const HW_PRESET_ULTRAFAST: i32 = 1;

const NVBUF_PAYLOAD_SURF_ARRAY: i32 = 0;
const NVBUF_LAYOUT_PITCH: i32 = 0;
const NVBUF_LAYOUT_BLOCK_LINEAR: i32 = 1;
const NVBUF_COLOR_ABGR32: i32 = 17;
const NVBUF_COLOR_XRGB32: i32 = 18;
const NVBUF_COLOR_NV12: i32 = 5;
const NVBUF_TAG_NONE: i32 = 0;
// The same ideas in the newer API, with the numbers read off the shipped headers rather than
// guessed (`nvbufsurface.h`, `nvbufsurftransform.h`, L4T R36.4.3).
const NVBUF_MEM_SURFACE_ARRAY: i32 = 4;
const NVBUF_SURF_COLOR_BGRX: i32 = 24;
const NVBUF_SURF_COLOR_RGBA: i32 = 19;
const NVBUF_SURF_COLOR_NV12: i32 = 6;
const NVBUF_SURF_TAG_VIDEO_ENC: i32 = 4608;
const NVBUF_SURF_TAG_NONE: i32 = 0;
const NVBUF_SURF_MAP_READ_WRITE: c_uint = 2;
const NVBUF_SURF_TRANSFORM_FILTER: u32 = 4;
const NVBUF_SURF_INTER_DEFAULT: i32 = 6;
const NVBUF_SURF_COMPUTE_VIC: i32 = 2;
const NVBUF_SURF_TRANSFORM_SUCCESS: c_int = 0;
const NVBUF_TAG_VIDEO_ENC: i32 = 4608;
const NVBUF_TRANSFORM_FILTER: u32 = 4;
const NVBUF_FILTER_SMART: u32 = 4;

const OUTPUT_BUFFERS: usize = 4;
const CAPTURE_BUFFERS: usize = 4;
/// How long the engine is given for an access unit it is expected to have. Its encode time is
/// well inside this on every board, so a wait that runs out means the unit is not coming until
/// more input does.
const OUTPUT_WAIT_MS: i32 = 100;
const ENCODER_NODES: [&str; 2] = ["/dev/v4l2-nvenc", "/dev/nvhost-msenc"];

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PlaneFormat {
    sizeimage: u32,
    bytesperline: u32,
    reserved: [u16; 6],
}

#[repr(C)]
#[derive(Clone, Copy)]
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
    reserved: u32,
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
    reserved: u32,
    _tail: u32,
}

/// `v4l2_streamparm` (204 bytes). Only the output plane's `timeperframe` is written; the rest is
/// laid out so that field lands where the driver reads it.
#[repr(C)]
struct StreamParm {
    type_: u32,
    capability: u32,
    output_mode: u32,
    numerator: u32,
    denominator: u32,
    _tail: [u8; 184],
}

/// Zeroed by hand: the tail is longer than the arrays `Default` is implemented for.
impl Default for StreamParm {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

#[repr(C)]
#[derive(Default)]
struct ExportBuffer {
    type_: u32,
    index: u32,
    plane: u32,
    flags: u32,
    fd: i32,
    reserved: [u32; 11],
}

#[repr(C, packed)]
struct ExtControl {
    id: u32,
    size: u32,
    reserved2: u32,
    value: i64,
}

#[repr(C)]
struct ExtControls {
    which: u32,
    count: u32,
    error_idx: u32,
    request_fd: i32,
    reserved: u32,
    _pad: u32,
    controls: *mut ExtControl,
}

#[repr(C)]
#[derive(Default)]
struct NvBufferCreateParams {
    width: i32,
    height: i32,
    payload_type: i32,
    memsize: i32,
    layout: i32,
    color_format: i32,
    nvbuf_tag: i32,
}

/// `NvBufSurfaceCreateParams` (32 bytes). `isContiguous` is a C `bool`, hence the padding.
#[repr(C)]
#[derive(Default)]
struct NvSurfCreateParams {
    gpu_id: u32,
    width: u32,
    height: u32,
    size: u32,
    is_contiguous: u8,
    _pad: [u8; 3],
    color_format: i32,
    layout: i32,
    mem_type: i32,
}

/// `NvBufSurfaceAllocateParams` (80 bytes): the create params plus the allocation tag, which is
/// what marks a surface as the video encoder's.
#[repr(C)]
struct NvSurfAllocateParams {
    params: NvSurfCreateParams,
    displayscanformat: i32,
    chroma_subsampling: i32,
    memtag: i32,
    disable_pitch_padding: u8,
    _pad0: [u8; 3],
    reserved_param: u32,
    _pad1: [u8; 4],
    reserved: [*mut c_void; 3],
}

/// `NvBufSurface` (64 bytes). Only `surface_list` and `num_filled` are touched.
#[repr(C)]
struct NvSurf {
    gpu_id: u32,
    batch_size: u32,
    num_filled: u32,
    is_contiguous: u8,
    _pad0: [u8; 3],
    mem_type: i32,
    _pad1: [u8; 4],
    surface_list: *mut NvSurfParams,
    reserved: [*mut c_void; 4],
}

/// `NvBufSurfaceParams` (384 bytes). Three fields are read: the pitch, the DMABUF descriptor the
/// encoder is fed, and the mapped address of plane 0. The rest is laid out only so the offsets of
/// those three land where the driver put them.
#[repr(C)]
struct NvSurfParams {
    width: u32,
    height: u32,
    pitch: u32,
    color_format: i32,
    layout: i32,
    _pad0: [u8; 4],
    buffer_desc: u64,
    data_size: u32,
    _pad1: [u8; 4],
    data_ptr: *mut c_void,
    _plane_params: [u8; 232],
    mapped_addr: [*mut c_void; 4],
    _mapped_tail: [u8; 40],
    paramex: *mut c_void,
    _reserved: [*mut c_void; 3],
}

/// `NvBufSurfTransformParams` (32 bytes).
#[repr(C)]
struct NvSurfTransformParams {
    transform_flag: u32,
    transform_flip: i32,
    transform_filter: i32,
    _pad: [u8; 4],
    src_rect: *mut c_void,
    dst_rect: *mut c_void,
}

/// `NvBufSurfTransformConfigParams` (16 bytes): which engine converts.
#[repr(C)]
struct NvSurfConfigParams {
    compute_mode: i32,
    gpu_id: i32,
    cuda_stream: *mut c_void,
}

impl Default for NvSurfAllocateParams {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

impl Default for NvSurfTransformParams {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

impl Default for NvSurfConfigParams {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

/// The surface format that matches the host frame's byte order. Checked on hardware by encoding
/// a frame of pure red and decoding it back: B,G,R,A bytes come out right as `XRGB32`, and the
/// mirrored R,G,B,A order as `ABGR32`, whatever the names suggest.
fn staging_format(rgba: bool) -> i32 {
    if rgba {
        NVBUF_COLOR_ABGR32
    } else {
        NVBUF_COLOR_XRGB32
    }
}

/// What a caller reads off a surface's first plane: the DMABUF the encoder is fed, and the
/// pitch and height to check the allocation against what was asked for.
struct SurfacePlane {
    desc: u64,
    pitch: usize,
    height: u32,
}

/// The sizes this interface is laid out for — the V4L2 half as the ioctl numbers above encode
/// them, the vendor half as its headers declare — against what this build lays out.
fn abi_matches() -> Result<(), String> {
    let expected = [
        ("v4l2_format", size_of::<Format>(), 208),
        ("v4l2_requestbuffers", size_of::<RequestBuffers>(), 20),
        ("v4l2_buffer", size_of::<Buffer>(), 88),
        ("v4l2_plane", size_of::<Plane>(), 64),
        ("v4l2_exportbuffer", size_of::<ExportBuffer>(), 64),
        ("v4l2_streamparm", size_of::<StreamParm>(), 204),
        ("v4l2_ext_control", size_of::<ExtControl>(), 20),
        ("v4l2_ext_controls", size_of::<ExtControls>(), 32),
        (
            "NvBufferCreateParams",
            size_of::<NvBufferCreateParams>(),
            28,
        ),
        (
            "NvBufSurfaceCreateParams",
            size_of::<NvSurfCreateParams>(),
            32,
        ),
        (
            "NvBufSurfaceAllocateParams",
            size_of::<NvSurfAllocateParams>(),
            80,
        ),
        ("NvBufSurface", size_of::<NvSurf>(), 64),
        ("NvBufSurfaceParams", size_of::<NvSurfParams>(), 384),
        (
            "NvBufSurfTransformParams",
            size_of::<NvSurfTransformParams>(),
            32,
        ),
        (
            "NvBufSurfTransformConfigParams",
            size_of::<NvSurfConfigParams>(),
            16,
        ),
    ];
    for (name, got, want) in expected {
        if got != want {
            return Err(format!(
                "{name} is {got} bytes here, the kernel ABI is {want}"
            ));
        }
    }
    Ok(())
}

/// Sizes and offsets of the external-RPS structures on one L4T release: R36 grew reserved and
/// per-codec fields, moving every size and `config_store` while the fields written here kept
/// their offsets. Read off `v4l2_nv_extensions.h` on a Nano at R32.6.1 and an AGX Orin at
/// R36.4.3; R35 was measured on no board and keeps the key frame per loss.
struct RpsLayout {
    /// `v4l2_enc_enable_ext_rps_ctr`.
    enable: usize,
    /// `v4l2_enc_num_ref_frames`, which the count is passed in: this control takes a pointer too.
    num_ref: usize,
    /// `v4l2_enc_frame_prop`, one entry of `RPSList`.
    prop: usize,
    /// `v4l2_enc_frame_ext_rps_ctrl_params`.
    params: usize,
    /// `v4l2_ctrl_videoenc_input_metadata`.
    metadata: usize,
    /// `config_store` in that structure: the index of the output buffer the frame is queued on.
    config_store: usize,
}

const RPS_R32: RpsLayout = RpsLayout {
    enable: 12,
    num_ref: 4,
    prop: 8,
    params: 84,
    metadata: 56,
    config_store: 48,
};
const RPS_R36: RpsLayout = RpsLayout {
    enable: 28,
    num_ref: 20,
    prop: 24,
    params: 232,
    metadata: 72,
    config_store: 56,
};

/// The layout for the release `/etc/nv_tegra_release` names; the surface library an image ships
/// says nothing about it.
fn rps_layout() -> Option<&'static RpsLayout> {
    static LAYOUT: OnceLock<Option<&'static RpsLayout>> = OnceLock::new();
    *LAYOUT.get_or_init(|| {
        let release = std::fs::read_to_string("/etc/nv_tegra_release").ok()?;
        rps_layout_for(&release)
    })
}

fn rps_layout_for(release: &str) -> Option<&'static RpsLayout> {
    // "# R36 (release), REVISION: 4.3, ..."
    let major = release
        .trim_start_matches(['#', ' '])
        .split([' ', '('])
        .next()?;
    match major {
        "R32" => Some(&RPS_R32),
        "R36" => Some(&RPS_R36),
        _ => None,
    }
}

/// A zeroed vendor structure with the fields this backend sets written at their offsets.
struct Fields(Vec<u8>);

impl Fields {
    fn new(size: usize) -> Self {
        Self(vec![0; size])
    }

    fn u8(&mut self, at: usize, value: u8) -> &mut Self {
        self.0[at] = value;
        self
    }

    fn u32(&mut self, at: usize, value: u32) -> &mut Self {
        self.0[at..at + 4].copy_from_slice(&value.to_ne_bytes());
        self
    }

    fn ptr(&mut self, at: usize, value: *const u8) -> &mut Self {
        self.0[at..at + 8].copy_from_slice(&(value as u64).to_ne_bytes());
        self
    }

    fn as_ptr(&self) -> *const u8 {
        self.0.as_ptr()
    }
}

type V4l2Open = unsafe extern "C" fn(*const c_char, c_int) -> c_int;
type V4l2Ioctl = unsafe extern "C" fn(c_int, u64, *mut c_void) -> c_int;
type V4l2Close = unsafe extern "C" fn(c_int) -> c_int;
type NvCreate = unsafe extern "C" fn(*mut c_int, *const NvBufferCreateParams) -> c_int;
type NvRaw2Buf = unsafe extern "C" fn(*const u8, c_uint, c_int, c_int, c_int) -> c_int;
type NvTransform = unsafe extern "C" fn(c_int, c_int, *mut c_void) -> c_int;
type NvDestroy = unsafe extern "C" fn(c_int) -> c_int;
type NvSurfAlloc = unsafe extern "C" fn(*mut *mut NvSurf, u32, *mut NvSurfAllocateParams) -> c_int;
type NvSurfDestroy = unsafe extern "C" fn(*mut NvSurf) -> c_int;
type NvSurfMap = unsafe extern "C" fn(*mut NvSurf, c_int, c_int, c_uint) -> c_int;
type NvSurfUnMap = unsafe extern "C" fn(*mut NvSurf, c_int, c_int) -> c_int;
type NvSurfSync = unsafe extern "C" fn(*mut NvSurf, c_int, c_int) -> c_int;
type NvSurfTransform =
    unsafe extern "C" fn(*mut NvSurf, *mut NvSurf, *mut NvSurfTransformParams) -> c_int;
type NvSurfSetSession = unsafe extern "C" fn(*mut NvSurfConfigParams) -> c_int;

/// The surface half of the vendor stack, which is where the two JetPack generations differ.
///
/// JetPack 4 ships `libnvbuf_utils.so`. JetPack 5 deprecated it in favor of `NvBufSurface`, and
/// JetPack 6 removed it: on an AGX Orin at L4T R36.4.3 there is no `libnvbuf_utils.so` at all,
/// only `libnvbufsurface.so` and `libnvbufsurftransform.so`. The encoder half is identical on
/// both — the same `libnvv4l2.so` and the same ioctls — so only this part is chosen at load time.
pub enum Surfaces {
    /// `nvbuf_utils`, JetPack 4.
    Utils(UtilsApi),
    /// `NvBufSurface`, JetPack 5 and 6.
    Surface(SurfaceApi),
}

pub struct UtilsApi {
    _lib: Library,
    create: NvCreate,
    raw2buf: NvRaw2Buf,
    transform: NvTransform,
    destroy: NvDestroy,
}

pub struct SurfaceApi {
    _surface: Library,
    _transform: Library,
    alloc: NvSurfAlloc,
    destroy: NvSurfDestroy,
    map: NvSurfMap,
    unmap: NvSurfUnMap,
    sync: NvSurfSync,
    transform: NvSurfTransform,
    set_session: NvSurfSetSession,
}

pub struct Vendor {
    _v4l2: Library,
    open: V4l2Open,
    ioctl: V4l2Ioctl,
    close: V4l2Close,
}

/// `dlopen` by plain name first, so the loader's own search applies, then the L4T directory for
/// images that do not put it on the default path.
fn open_lib(name: &str) -> Result<Library, String> {
    unsafe {
        Library::new(name)
            .or_else(|_| Library::new(format!("/usr/lib/aarch64-linux-gnu/tegra/{name}")))
    }
    .map_err(|e| format!("{name} is not loadable: {e}"))
}

/// One symbol as a bare pointer, for the caller to transmute to the signature the vendor
/// documents for it. Every call here is unsafe on its own terms, which is what the 2024 edition
/// asks for and what marks the lines that trust the vendor's ABI.
fn sym(lib: &Library, name: &[u8]) -> Result<*const c_void, String> {
    let s: Symbol<*const c_void> = unsafe { lib.get(name) }
        .map_err(|e| format!("{} is missing: {e}", String::from_utf8_lossy(name)))?;
    Ok(*s)
}

impl Vendor {
    /// Load the encoder library alone.
    unsafe fn load() -> Result<Self, String> {
        let v4l2 = open_lib("libnvv4l2.so")?;
        let (open, ioctl, close) = (
            sym(&v4l2, b"v4l2_open\0")?,
            sym(&v4l2, b"v4l2_ioctl\0")?,
            sym(&v4l2, b"v4l2_close\0")?,
        );
        Ok(Self {
            open: unsafe { std::mem::transmute::<*const c_void, V4l2Open>(open) },
            ioctl: unsafe { std::mem::transmute::<*const c_void, V4l2Ioctl>(ioctl) },
            close: unsafe { std::mem::transmute::<*const c_void, V4l2Close>(close) },
            _v4l2: v4l2,
        })
    }
}

/// The surface library this JetPack carries, loaded when a session is built rather than when the
/// backend is probed.
///
/// **Not loaded by `available()`, on purpose.** `libnvbufsurftransform.so` brings EGL up in its
/// ELF constructor, EGL talks to the X server through libxcb, and in a process whose libxcb came
/// from somewhere other than the system — an AppImage bundling its own — that took the process
/// down with SIGSEGV inside `dlopen`, on the worker thread that had merely asked whether this
/// host has an encoder.
fn surfaces() -> Option<&'static Surfaces> {
    static SURFACES: OnceLock<Option<Surfaces>> = OnceLock::new();
    SURFACES
        .get_or_init(|| match load_surfaces() {
            Ok(surfaces) => Some(surfaces),
            Err(e) => {
                crate::log::debug!("[pixelflux] Tegra surface API unavailable: {e}");
                None
            }
        })
        .as_ref()
}

/// The older library is tried first, because a JetPack 5 image can carry both and its
/// `nvbuf_utils` is the one this backend has the most hours on.
fn load_surfaces() -> Result<Surfaces, String> {
    match open_lib("libnvbuf_utils.so") {
        Ok(nvbuf) => {
            let (create, raw2buf, transform, destroy) = (
                sym(&nvbuf, b"NvBufferCreateEx\0")?,
                sym(&nvbuf, b"Raw2NvBuffer\0")?,
                sym(&nvbuf, b"NvBufferTransform\0")?,
                sym(&nvbuf, b"NvBufferDestroy\0")?,
            );
            Ok(Surfaces::Utils(UtilsApi {
                create: unsafe { std::mem::transmute::<*const c_void, NvCreate>(create) },
                raw2buf: unsafe { std::mem::transmute::<*const c_void, NvRaw2Buf>(raw2buf) },
                transform: unsafe { std::mem::transmute::<*const c_void, NvTransform>(transform) },
                destroy: unsafe { std::mem::transmute::<*const c_void, NvDestroy>(destroy) },
                _lib: nvbuf,
            }))
        }
        Err(utils_error) => {
            let surface = open_lib("libnvbufsurface.so")
                .map_err(|e| format!("neither surface API is loadable: {utils_error}; {e}"))?;
            let xform = open_lib("libnvbufsurftransform.so")?;
            let (alloc, destroy, map, unmap, sync) = (
                sym(&surface, b"NvBufSurfaceAllocate\0")?,
                sym(&surface, b"NvBufSurfaceDestroy\0")?,
                sym(&surface, b"NvBufSurfaceMap\0")?,
                sym(&surface, b"NvBufSurfaceUnMap\0")?,
                sym(&surface, b"NvBufSurfaceSyncForDevice\0")?,
            );
            let (transform, set_session) = (
                sym(&xform, b"NvBufSurfTransform\0")?,
                sym(&xform, b"NvBufSurfTransformSetSessionParams\0")?,
            );
            Ok(Surfaces::Surface(SurfaceApi {
                alloc: unsafe { std::mem::transmute::<*const c_void, NvSurfAlloc>(alloc) },
                destroy: unsafe { std::mem::transmute::<*const c_void, NvSurfDestroy>(destroy) },
                map: unsafe { std::mem::transmute::<*const c_void, NvSurfMap>(map) },
                unmap: unsafe { std::mem::transmute::<*const c_void, NvSurfUnMap>(unmap) },
                sync: unsafe { std::mem::transmute::<*const c_void, NvSurfSync>(sync) },
                transform: unsafe {
                    std::mem::transmute::<*const c_void, NvSurfTransform>(transform)
                },
                set_session: unsafe {
                    std::mem::transmute::<*const c_void, NvSurfSetSession>(set_session)
                },
                _surface: surface,
                _transform: xform,
            }))
        }
    }
}

/// The capture-queue format for a codec the vendor encoder can be set to, or `None` for one it
/// has no format for. The queues, controls, and surface formats are the same whichever of these
/// the capture queue carries, so the codec is a parameter rather than a second backend.
pub fn coded_fourcc(codec: Codec) -> Option<u32> {
    match codec {
        Codec::H264 => Some(V4L2_PIX_FMT_H264),
        Codec::H265 => Some(V4L2_PIX_FMT_H265),
        Codec::Av1 => Some(V4L2_PIX_FMT_AV1),
        _ => None,
    }
}

/// Every codec the vendor encoder can be set to, less AV1 on the generations whose engine has
/// none. The node enumerates that format on a Tegra X1 all the same and refuses it only once a
/// session asks, so the device tree's SoC name decides; the board stays the authority for the
/// rest at `S_FMT`, where a refusal costs one attempt and the codec's software encoder, and a
/// board this does not know keeps AV1, since a codec withheld here has no route back to hardware
/// that does carry it.
pub fn served() -> Vec<Codec> {
    static COMPATIBLE: OnceLock<Vec<u8>> = OnceLock::new();
    served_on(
        COMPATIBLE
            .get_or_init(|| std::fs::read("/proc/device-tree/compatible").unwrap_or_default()),
    )
}

/// `served` for a board whose device tree `compatible` list, NUL-separated `nvidia,<name>`
/// entries, is `compatible`: AV1 is withheld where it names a Tegra X1, X2, or Xavier SoC.
fn served_on(compatible: &[u8]) -> Vec<Codec> {
    let without_av1 = compatible
        .split(|&b| b == 0)
        .filter_map(|entry| std::str::from_utf8(entry).ok()?.rsplit(',').next())
        .any(|soc| {
            ["tegra21", "tegra18", "tegra19"]
                .iter()
                .any(|generation| soc.starts_with(generation))
        });
    let mut served = vec![Codec::H264, Codec::H265];
    if !without_av1 {
        served.push(Codec::Av1);
    }
    served
}

/// Whether this host has the Tegra encoder: the encoder library loads and an encoder node is
/// there. Probed once, because a failure here is a property of the machine and not of the
/// session.
///
/// **Nothing vendor-side is brought up here**, and each half of that cost a crash to learn. The
/// surface library initializes EGL in its ELF constructor, which reaches the X server through
/// libxcb and takes down a process whose libxcb came from elsewhere. Opening the node hands the
/// shim the encoder block, which it will not give up while the surface library is absent: a probe
/// that opened it reported no encoder at all on JetPack 4, and on JetPack 6 the process aborted
/// in `Py_Finalize` afterwards. A `stat` answers what the callers ask — and they ask early, from
/// whatever thread they like. A node that exists but will not open is then an init failure like
/// any other, and the ladder's own line says what took over.
pub fn available() -> bool {
    static PROBED: OnceLock<bool> = OnceLock::new();
    *PROBED.get_or_init(|| {
        if vendor().is_none() {
            return false;
        }
        ENCODER_NODES.iter().any(|node| {
            let Ok(path) = CString::new(*node) else {
                return false;
            };
            let mut info: libc::stat = unsafe { std::mem::zeroed() };
            let found = unsafe { libc::stat(path.as_ptr(), &mut info) } == 0;
            found && (info.st_mode & libc::S_IFMT) == libc::S_IFCHR
        })
    })
}

/// The vendor libraries, loaded once for the process. They do not survive being unloaded and
/// loaded again: a second `dlopen` after the first handle is dropped fails, and a session then
/// falls back to software, which the ladder's own refusal line reports.
fn vendor() -> Option<&'static Vendor> {
    static VENDOR: OnceLock<Option<Vendor>> = OnceLock::new();
    VENDOR
        .get_or_init(|| match unsafe { Vendor::load() } {
            Ok(vendor) => Some(vendor),
            Err(e) => {
                crate::log::debug!("[pixelflux] Tegra vendor libraries unavailable: {e}");
                None
            }
        })
        .as_ref()
}

pub struct TegraEncoder {
    vendor: &'static Vendor,
    surfaces: &'static Surfaces,
    fd: c_int,
    width: i32,
    height: i32,
    row_bytes: usize,
    staging_fd: c_int,
    nv12_fd: [c_int; OUTPUT_BUFFERS],
    /// Filled on the `NvBufSurface` path only: that API takes surfaces where the older one takes
    /// descriptors, and the DMABUF the encoder is fed is the surface's own `bufferDesc`. The
    /// staging plane is mapped once for the life of the session, because there is no
    /// `Raw2NvBuffer` to copy a host frame in.
    staging_surf: *mut NvSurf,
    nv12_surf: [*mut NvSurf; OUTPUT_BUFFERS],
    staging_map: *mut u8,
    staging_pitch: usize,
    capture: [(*mut c_void, usize); CAPTURE_BUFFERS],
    queued: usize,
    /// Access units queued and not yet collected. The engine emits a frame once the next is
    /// queued behind it, so one is expected only from `wait_from` in flight, learned from the
    /// waits that ran out rather than assumed of a generation.
    outstanding: usize,
    wait_from: usize,
    /// The newest frame of its own, whose unit is still inside, and the slot it was converted
    /// into: a still screen repeats it to bring the unit out.
    held: Option<(u64, usize)>,
    scratch: Vec<u8>,
    codec: Codec,
    omit_headers: bool,
    bitrate_bps: u32,
    fps: f64,
    /// The references the decoder holds, where the encoder takes the reference set from the
    /// session (`rps`); None where it chooses its own and a lost frame costs a key frame.
    references: Option<ReferenceWindow>,
    rps: Option<&'static RpsLayout>,
    /// How many frames the encoder may predict from, which is what the window holds.
    dpb: u32,
    /// What each frame in flight predicted from, by frame number, in the order queued: the
    /// encoder hands a unit back one or two frames after the one that produced it.
    in_flight: VecDeque<(u64, Reference)>,
    /// The units the last call handed back, in order, as `delivered_units` describes them.
    units: Vec<(u16, Reference, usize)>,
    last_reference: Reference,
    /// Frames between the key frames the session asks for itself, and how many since the last.
    keyframe_every: u64,
    since_key: u64,
    /// Set when the encoder coded a key frame nobody asked for: its references and the window's
    /// no longer agree, and the next frame is a key frame to make them agree again.
    resync: bool,
    /// The reorder bound the H.264 sets are held to where the stream rules reordering out: the
    /// session asks for picture order count type 2, which a driver may refuse, and hands units
    /// back one or two frames late, so the session alone does not prove the order.
    reorder: super::sps::NoReorder,
}

impl TegraEncoder {
    pub fn new(codec: Codec, settings: &CaptureSettings, rgba: bool) -> Result<Self, String> {
        abi_matches()?;
        let coded = coded_fourcc(codec)
            .ok_or_else(|| format!("the encoder does not serve {}", codec.display()))?;
        let (width, height) = (settings.width, settings.height);
        let fps = settings.target_fps.max(1.0);
        let bitrate_bps = (settings.video_bitrate_kbps.max(1) as u32).saturating_mul(1000);
        if width <= 0 || height <= 0 || width % 2 != 0 || height % 2 != 0 {
            return Err(format!(
                "the encoder needs even dimensions, got {width}x{height}"
            ));
        }
        let vendor = vendor().ok_or("the Tegra vendor libraries are unavailable")?;
        // The surface library loads before the node is opened, and that order is the shim's, not
        // a preference: opened first, the node does not open at all on JetPack 4, and on
        // JetPack 6 the process aborted in `Py_Finalize` afterwards.
        let surfaces = surfaces().ok_or("the Tegra surface libraries are unavailable")?;

        let mut fd = -1;
        let mut opened = "";
        for node in ENCODER_NODES {
            let path = CString::new(node).unwrap();
            let candidate =
                unsafe { (vendor.open)(path.as_ptr(), libc::O_RDWR | libc::O_NONBLOCK) };
            if candidate >= 0 {
                fd = candidate;
                opened = node;
                break;
            }
        }
        if fd < 0 {
            return Err(format!("no encoder node opened, tried {ENCODER_NODES:?}"));
        }

        let mut me = Self {
            vendor,
            surfaces,
            fd,
            width,
            height,
            row_bytes: width as usize * 4,
            staging_fd: -1,
            nv12_fd: [-1; OUTPUT_BUFFERS],
            staging_surf: ptr::null_mut(),
            nv12_surf: [ptr::null_mut(); OUTPUT_BUFFERS],
            staging_map: ptr::null_mut(),
            staging_pitch: 0,
            capture: [(ptr::null_mut(), 0); CAPTURE_BUFFERS],
            queued: 0,
            outstanding: 0,
            wait_from: 1,
            held: None,
            scratch: Vec::new(),
            codec,
            omit_headers: settings.omit_stripe_headers,
            bitrate_bps,
            fps,
            references: None,
            rps: None,
            // The level the encoder picks admits no more than this: at 3840x2160 a set of eight
            // fails the session (`BlockSide error 0x4`) where level 5.1's five do not.
            dpb: match codec {
                Codec::H265 => h265_dpb_frames(153, width as u32, height as u32),
                _ => h264_dpb_frames(51, width as u32, height as u32),
            },
            in_flight: VecDeque::new(),
            units: Vec::new(),
            last_reference: Reference::Untracked,
            keyframe_every: 0,
            since_key: 0,
            resync: false,
            reorder: super::sps::NoReorder::new("The Tegra encoder", false),
        };
        if let Err(e) = me.setup(settings, fps, bitrate_bps, rgba, coded) {
            return Err(format!("{opened}: {e}"));
        }
        Ok(me)
    }

    fn ioctl<T>(&self, request: u64, arg: &mut T, what: &str) -> Result<(), String> {
        let rc = unsafe { (self.vendor.ioctl)(self.fd, request, arg as *mut T as *mut c_void) };
        if rc < 0 {
            return Err(format!(
                "{what} failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    /// One surface out of `NvBufSurfaceAllocate`, its batch marked as carrying a frame, and its
    /// first plane taken. The caller's field is written only once the surface is one `Drop` can
    /// destroy, so an allocation the library refused leaves nothing behind to free.
    fn allocate_surface(
        api: &SurfaceApi,
        params: &mut NvSurfAllocateParams,
        what: &str,
    ) -> Result<(*mut NvSurf, SurfacePlane), String> {
        let mut surf: *mut NvSurf = ptr::null_mut();
        if unsafe { (api.alloc)(&mut surf, 1, params) } < 0 {
            return Err(format!("NvBufSurfaceAllocate for {what} failed"));
        }
        let Some(batch) = (unsafe { surf.as_mut() }) else {
            return Err(format!(
                "NvBufSurfaceAllocate for {what} returned no surface"
            ));
        };
        // `numFilled` is what the transform reads to know the batch carries a frame.
        batch.num_filled = 1;
        let Some(plane) = (unsafe { batch.surface_list.as_ref() }) else {
            unsafe { (api.destroy)(surf) };
            return Err(format!("{what} came back carrying no plane"));
        };
        Ok((
            surf,
            SurfacePlane {
                desc: plane.buffer_desc,
                pitch: plane.pitch as usize,
                height: plane.height,
            },
        ))
    }

    /// Where the staging plane is mapped, or `None` if the batch, its plane, or the mapping is
    /// absent. Read after `NvBufSurfaceMap`, which is what fills it in.
    fn mapped_plane(surf: *mut NvSurf) -> Option<*mut c_void> {
        let batch = unsafe { surf.as_ref() }?;
        let plane = unsafe { batch.surface_list.as_ref() }?;
        let mapped = plane.mapped_addr[0];
        (!mapped.is_null()).then_some(mapped)
    }

    /// Allocate the two kinds of surface this path needs: one pitch-linear surface in the host
    /// frame's byte order, and one block-linear NV12 surface per queued encoder buffer.
    ///
    /// Both vendor APIs are asked for the same thing and both hand back a DMABUF descriptor —
    /// `NvBufferCreateEx` directly, `NvBufSurfaceAllocate` as the surface's `bufferDesc`, which is
    /// what the vendor's own C++ wrapper returns as its `fd`. On the newer API the staging plane
    /// is also mapped here and kept mapped: it has no `Raw2NvBuffer`, so the rows are written by
    /// this session.
    fn allocate_surfaces(&mut self, rgba: bool) -> Result<(), String> {
        match self.surfaces {
            Surfaces::Utils(api) => {
                let mut staging = NvBufferCreateParams {
                    width: self.width,
                    height: self.height,
                    payload_type: NVBUF_PAYLOAD_SURF_ARRAY,
                    memsize: 0,
                    layout: NVBUF_LAYOUT_PITCH,
                    color_format: staging_format(rgba),
                    nvbuf_tag: NVBUF_TAG_NONE,
                };
                if unsafe { (api.create)(&mut self.staging_fd, &staging) } < 0 {
                    return Err("NvBufferCreateEx for the staging surface failed".into());
                }
                staging.layout = NVBUF_LAYOUT_BLOCK_LINEAR;
                staging.color_format = NVBUF_COLOR_NV12;
                staging.nvbuf_tag = NVBUF_TAG_VIDEO_ENC;
                for slot in 0..OUTPUT_BUFFERS {
                    if unsafe { (api.create)(&mut self.nv12_fd[slot], &staging) } < 0 {
                        return Err("NvBufferCreateEx for an NV12 surface failed".into());
                    }
                }
                Ok(())
            }
            Surfaces::Surface(api) => {
                // Pinned to the VIC block. The default engine and the VIC both cost 4.9 ms a
                // frame at 1080p on an AGX Orin and the GPU 0.9 ms, but that GPU is busy with
                // the work the board exists for.
                let mut cfg = NvSurfConfigParams {
                    compute_mode: NVBUF_SURF_COMPUTE_VIC,
                    ..Default::default()
                };
                if unsafe { (api.set_session)(&mut cfg) } != NVBUF_SURF_TRANSFORM_SUCCESS {
                    eprintln!(
                        "[pixelflux] Tegra: the transform session would not take the VIC; using its default engine."
                    );
                }

                let mut params = NvSurfAllocateParams::default();
                params.params.width = self.width as u32;
                params.params.height = self.height as u32;
                params.params.layout = NVBUF_LAYOUT_PITCH;
                params.params.color_format = if rgba {
                    NVBUF_SURF_COLOR_RGBA
                } else {
                    NVBUF_SURF_COLOR_BGRX
                };
                params.params.mem_type = NVBUF_MEM_SURFACE_ARRAY;
                params.memtag = NVBUF_SURF_TAG_NONE;
                let (surf, plane) =
                    Self::allocate_surface(api, &mut params, "the staging surface")?;
                self.staging_surf = surf;
                self.staging_fd = plane.desc as c_int;
                self.staging_pitch = plane.pitch;
                if plane.pitch < self.row_bytes || plane.height != self.height as u32 {
                    return Err(format!(
                        "staging surface reports pitch {} height {} for {}x{}",
                        plane.pitch, plane.height, self.width, self.height
                    ));
                }
                if unsafe { (api.map)(surf, 0, 0, NVBUF_SURF_MAP_READ_WRITE) } < 0 {
                    return Err("NvBufSurfaceMap of the staging surface failed".into());
                }
                let mapped = Self::mapped_plane(surf)
                    .ok_or("the staging surface mapped to a null address")?;
                self.staging_map = mapped as *mut u8;

                params.params.layout = NVBUF_LAYOUT_BLOCK_LINEAR;
                params.params.color_format = NVBUF_SURF_COLOR_NV12;
                params.memtag = NVBUF_SURF_TAG_VIDEO_ENC;
                for slot in 0..OUTPUT_BUFFERS {
                    let (surf, plane) =
                        Self::allocate_surface(api, &mut params, "an NV12 surface")?;
                    self.nv12_surf[slot] = surf;
                    self.nv12_fd[slot] = plane.desc as c_int;
                }
                Ok(())
            }
        }
    }

    /// Put one host frame into the staging surface.
    fn fill_staging(&mut self, pixels: &[u8], stride: usize) -> Result<(), String> {
        let height = self.height as usize;
        match self.surfaces {
            Surfaces::Utils(api) => {
                // Raw2NvBuffer takes tight rows, so a padded frame is packed once into scratch.
                let source = if stride == self.row_bytes {
                    pixels
                } else {
                    self.scratch.resize(self.row_bytes * height, 0);
                    for row in 0..height {
                        let from = row * stride;
                        self.scratch[row * self.row_bytes..(row + 1) * self.row_bytes]
                            .copy_from_slice(&pixels[from..from + self.row_bytes]);
                    }
                    &self.scratch
                };
                if unsafe {
                    (api.raw2buf)(source.as_ptr(), 0, self.width, self.height, self.staging_fd)
                } < 0
                {
                    return Err("Raw2NvBuffer failed".into());
                }
                Ok(())
            }
            Surfaces::Surface(api) => {
                // The rows go straight to their place in the mapped plane, so a padded input
                // costs nothing extra here, and the plane is synced for the device once.
                for row in 0..height {
                    unsafe {
                        ptr::copy_nonoverlapping(
                            pixels.as_ptr().add(row * stride),
                            self.staging_map.add(row * self.staging_pitch),
                            self.row_bytes,
                        );
                    }
                }
                if unsafe { (api.sync)(self.staging_surf, 0, 0) } < 0 {
                    return Err("NvBufSurfaceSyncForDevice failed".into());
                }
                Ok(())
            }
        }
    }

    /// Convert the staging surface into the NV12 surface of one output slot, on the VIC.
    fn convert_to_nv12(&mut self, slot: usize) -> Result<(), String> {
        match self.surfaces {
            Surfaces::Utils(api) => {
                let mut params = [0u8; 56];
                params[0..4].copy_from_slice(&NVBUF_TRANSFORM_FILTER.to_ne_bytes());
                params[8..12].copy_from_slice(&NVBUF_FILTER_SMART.to_ne_bytes());
                if unsafe {
                    (api.transform)(
                        self.staging_fd,
                        self.nv12_fd[slot],
                        params.as_mut_ptr() as *mut c_void,
                    )
                } < 0
                {
                    return Err("NvBufferTransform failed".into());
                }
                Ok(())
            }
            Surfaces::Surface(api) => {
                let mut params = NvSurfTransformParams {
                    transform_flag: NVBUF_SURF_TRANSFORM_FILTER,
                    transform_filter: NVBUF_SURF_INTER_DEFAULT,
                    ..Default::default()
                };
                if unsafe { (api.transform)(self.staging_surf, self.nv12_surf[slot], &mut params) }
                    != NVBUF_SURF_TRANSFORM_SUCCESS
                {
                    return Err("NvBufSurfTransform failed".into());
                }
                Ok(())
            }
        }
    }

    /// Tell the encoder the frame rate it encodes at.
    ///
    /// Without it the driver rate-controls against its own default rate, so every number derived
    /// from the real one — the CBR bit budget above all — is spent against the wrong rate.
    /// `timeperframe` is a period, hence `1000 / (fps * 1000)`, which keeps a fractional rate
    /// like 29.97 exact instead of rounding it into the denominator.
    fn set_frame_rate(&mut self, fps: f64) -> Result<(), String> {
        let mut parm = StreamParm {
            type_: V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE,
            numerator: 1000,
            denominator: (fps.max(1.0) * 1000.0).round() as u32,
            ..Default::default()
        };
        self.ioctl(VIDIOC_S_PARM, &mut parm, "S_PARM output")?;
        self.fps = fps;
        Ok(())
    }

    /// Set one encoder control. `VIRTUALBUFFER_SIZE` is compound: the driver reads the value
    /// through a pointer in the same union, and passing it inline makes it read whatever address
    /// the number happens to name, which silently wrecks the rest of the configuration.
    fn set_control(&self, id: u32, value: i64, what: &str) -> Result<(), String> {
        let mut vbv = value as u32;
        let value = if id == CID_VBV_SIZE {
            &mut vbv as *mut u32 as i64
        } else {
            value
        };
        let mut control = ExtControl {
            id,
            size: 0,
            reserved2: 0,
            value,
        };
        let mut controls = ExtControls {
            which: V4L2_CTRL_CLASS_MPEG,
            count: 1,
            error_idx: 0,
            request_fd: 0,
            reserved: 0,
            _pad: 0,
            controls: &mut control,
        };
        self.ioctl(VIDIOC_S_EXT_CTRLS, &mut controls, what)
    }

    /// Set a compound control, whose value the driver reads through the pointer in the union.
    fn set_pointer_control(&self, id: u32, value: *const u8, what: &str) -> Result<(), String> {
        let mut control = ExtControl {
            id,
            size: 0,
            reserved2: 0,
            value: value as i64,
        };
        let mut controls = ExtControls {
            which: V4L2_CTRL_CLASS_MPEG,
            count: 1,
            error_idx: 0,
            request_fd: 0,
            reserved: 0,
            _pad: 0,
            controls: &mut control,
        };
        self.ioctl(VIDIOC_S_EXT_CTRLS, &mut controls, what)
    }

    /// Take the reference set into the session's hands, so a frame a client lost is left out of
    /// it instead of answered with a key frame. Before the buffers are requested, which is what
    /// the vendor's `enableExternalRPS` checks; once on, a frame queued without its set fails the
    /// session (`BlockSide error 0x4`, then `EINVAL` on the next `QBUF`), so `rps` is set only
    /// here and every queued frame carries one.
    ///
    /// H.265 only: under an external set the vendor's H.264 encoder codes pictures its own
    /// decoder does not reconstruct where content repeats within the search range, at any list
    /// order, reference count, POC type, or preset, as NVIDIA's own sample shows, so an H.264
    /// session keeps the key frame per loss. AV1 carries different metadata altogether.
    fn enable_external_rps(&mut self) {
        let (at, bits) = match self.codec {
            Codec::H265 => (8, H265_POC_LSB_BITS),
            _ => return,
        };
        let Some(layout) = rps_layout() else {
            crate::log::debug!(
                "[pixelflux] Tegra: no external RPS layout known for this L4T release"
            );
            return;
        };
        let mut enable = Fields::new(layout.enable);
        enable.u8(0, 1).u32(at, bits);
        if let Err(e) = self.set_pointer_control(CID_EXTERNAL_RPS, enable.as_ptr(), "external RPS")
        {
            crate::log::debug!("[pixelflux] Tegra: {e}; a lost frame costs a key frame");
            return;
        }
        // The count follows the enable and is a compound control too: passed inline, the driver
        // reads the number as an address and dies inside the vendor library. Refused, the
        // encoder keeps its default of one, and the window holds what the encoder does.
        let mut count = Fields::new(layout.num_ref);
        count.u32(0, self.dpb);
        if let Err(e) =
            self.set_pointer_control(CID_NUM_REFERENCE_FRAMES, count.as_ptr(), "reference frames")
        {
            eprintln!("[pixelflux] Tegra: {e}; predicting from one reference frame");
            self.dpb = 1;
        }
        self.rps = Some(layout);
        self.references = Some(ReferenceWindow::new(self.dpb));
    }

    /// Name the references the frame about to be queued on `slot` may predict from, and note
    /// what that makes it predict from. An empty set is a key frame — the first, one asked for,
    /// one the window needs because nothing it holds is left, and the periodic one, coded here
    /// since the encoder's own interval is parked.
    fn queue_references(
        &mut self,
        layout: &RpsLayout,
        slot: usize,
        frame_number: u64,
        force_idr: bool,
    ) -> Result<(), String> {
        let references = self
            .references
            .as_ref()
            .ok_or("the reference window is missing")?;
        let key = force_idr
            || self.resync
            || self.since_key >= self.keyframe_every
            || !references.has_reference();
        let held: Vec<u64> = if key {
            Vec::new()
        } else {
            references.held().filter(|f| !f.2).map(|f| f.1).collect()
        };
        let held = &held[held.len().saturating_sub(RPS_LIST_LEN)..];

        // The encoder's own ids are the window's timestamps, which do not wrap the way the frame
        // number on the wire does.
        let mut params = Fields::new(layout.params);
        params
            .u32(0, references.next_pts() as u32)
            .u8(4, 1)
            .u32(8, self.dpb)
            .u32(12, held.len() as u32)
            .u32(16, held.last().map_or(0, |&pts| pts as u32));
        for (i, &pts) in held.iter().enumerate() {
            params.u32(20 + i * layout.prop, pts as u32);
        }
        let mut metadata = Fields::new(layout.metadata);
        metadata
            .u32(0, INPUT_METADATA_RPS)
            .ptr(32, params.as_ptr())
            .u32(layout.config_store, slot as u32);
        self.set_pointer_control(CID_INPUT_METADATA, metadata.as_ptr(), "frame references")?;

        let reference = self
            .references
            .as_mut()
            .map_or(Reference::Untracked, |w| w.record(frame_number as u16, key));
        // A unit comes back one or two frames later; one the encoder never returns must not pin
        // the rest of the queue behind it.
        if self.in_flight.len() >= OUTPUT_BUFFERS + CAPTURE_BUFFERS {
            self.in_flight.pop_front();
        }
        self.in_flight.push_back((frame_number, reference));
        if key {
            self.since_key = 1;
            self.resync = false;
        } else {
            self.since_key += 1;
        }
        Ok(())
    }

    /// What the unit carrying frame `number` predicted from, where the session names references.
    fn reference_of(&mut self, number: u64) -> Reference {
        while let Some((queued, reference)) = self.in_flight.pop_front() {
            if queued == number {
                return reference;
            }
        }
        Reference::Untracked
    }

    /// A key frame the session did not ask for means the encoder dropped references the window
    /// still lists, so the next frame is a key frame to make them agree again.
    fn note_key_frame(&mut self, reference: Reference) {
        if self.references.is_some() && reference != Reference::None {
            eprintln!(
                "[pixelflux] Tegra: the encoder coded a key frame the session did not ask for; resynchronizing its references"
            );
            self.resync = true;
        }
    }

    /// The frame the first unit the last call handed back predicted from; `Untracked` where the
    /// session names no references or the call handed back nothing.
    pub fn last_reference(&self) -> Reference {
        self.last_reference
    }

    /// The units the last call handed back: each one's frame number, what it predicted from, and
    /// where it ends in the data. A unit comes back a frame or two after the frame that produced
    /// it, several at once after a stall, so a consumer that drops one reports it lost by that
    /// unit's own frame number; named otherwise, the lost frame stays a reference.
    pub fn delivered_units(&self) -> &[(u16, Reference, usize)] {
        &self.units
    }

    /// Leave frame `frame_id` and every frame after it out of the predictions. The encoder
    /// learns of it with the next frame's reference set, which the window now leaves it out of,
    /// or which is empty, making that frame a key frame. False where the session does not name
    /// references, and the caller codes a key frame instead.
    pub fn invalidate_reference(&mut self, frame_id: u16) -> bool {
        match &mut self.references {
            Some(references) => {
                references.invalidate(frame_id);
                true
            }
            None => false,
        }
    }

    fn format(&self, type_: u32, pixelformat: u32, planes: u8, sizeimage: u32) -> Format {
        let mut plane_fmt = [PlaneFormat::default(); 8];
        plane_fmt[0].sizeimage = sizeimage;
        Format {
            type_,
            _pad: 0,
            pix_mp: PixFormatMplane {
                width: self.width as u32,
                height: self.height as u32,
                pixelformat,
                field: 1,
                colorspace: 0,
                plane_fmt,
                num_planes: planes,
                flags: 0,
                enc: 0,
                quantization: 0,
                xfer_func: 0,
                reserved: [0; 7],
            },
            _tail: [0; 8],
        }
    }

    fn setup(
        &mut self,
        settings: &CaptureSettings,
        fps: f64,
        bitrate_bps: u32,
        rgba: bool,
        coded: u32,
    ) -> Result<(), String> {
        let pixels = self.width as u32 * self.height as u32;
        let mut capture_format = self.format(
            V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
            coded,
            1,
            pixels.max(2 << 20),
        );
        self.ioctl(VIDIOC_S_FMT, &mut capture_format, "S_FMT capture")?;
        let mut output_format =
            self.format(V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE, V4L2_PIX_FMT_NV12M, 2, 0);
        self.ioctl(VIDIOC_S_FMT, &mut output_format, "S_FMT output")?;
        self.set_frame_rate(fps)?;

        // No infinite GOP here either: a session that asks for none gets ten seconds, long enough
        // not to spend bitrate on key frames and short enough to bound recovery when one is lost.
        let seconds = if settings.keyframe_interval_s > 0.0 {
            settings.keyframe_interval_s
        } else {
            10.0
        };
        let keyframe = ((fps * seconds) as i64).clamp(1, 600);
        let vbv = (bitrate_bps as f64 / fps.max(1.0)) as i64;
        self.set_control(CID_BITRATE, bitrate_bps as i64, "bitrate")?;
        self.set_control(
            CID_BITRATE_MODE,
            BITRATE_MODE_CBR as i64,
            "rate control mode",
        )?;
        // AV1 has no profile control in the vendor header; the other two carry their own.
        let profile = match self.codec {
            Codec::H264 => Some((CID_H264_PROFILE, H264_PROFILE_MAIN)),
            Codec::H265 => Some((CID_H265_PROFILE, H265_PROFILE_MAIN)),
            _ => None,
        };
        if let Some((cid, value)) = profile {
            self.set_control(cid, value as i64, "profile")?;
        }
        self.set_control(CID_HW_PRESET, HW_PRESET_ULTRAFAST as i64, "preset")?;
        self.set_control(CID_MAX_PERFORMANCE, 1, "max performance")?;
        // Every IDR carries what a client needs to start on it: parameter sets for H.264 and
        // H.265, the sequence header for AV1, which the vendor puts behind its own control. VUI
        // is an H.264 and H.265 construct, and AV1 declares its color in that sequence header.
        let headers = match self.codec {
            Codec::Av1 => CID_AV1_HEADERS_WITH_FRAME,
            _ => CID_INSERT_SPS_PPS_AT_IDR,
        };
        self.set_control(headers, 1, "sequence headers at IDR")?;
        if self.codec != Codec::Av1 {
            self.set_control(CID_INSERT_VUI, 1, "VUI")?;
        }
        // Picture order count type is an H.264 field with no H.265 counterpart, and NVIDIA
        // documents even the H.264 control as Orin-only on current L4T though older drivers
        // take it. It saves a few bits per slice on a stream that never reorders and nothing
        // else, so a driver that refuses it is not worth the session's hardware encoder.
        if self.codec == Codec::H264 {
            let _ = self.set_control(CID_POC_TYPE, 2, "picture order count type");
        }
        // With the reference set the session's, so is every key frame: one the encoder inserts
        // on an interval of its own, IDR or GOP, is coded against the set it was handed, and the
        // frames after it decode as noise until the window learns of it. Both intervals are
        // parked and the session asks for each key frame with an empty set.
        self.keyframe_every = keyframe as u64;
        self.enable_external_rps();
        let idr_interval = if self.rps.is_some() {
            i32::MAX as i64
        } else {
            keyframe
        };
        self.set_control(CID_IDR_INTERVAL, idr_interval, "IDR interval")?;
        if self.rps.is_some() {
            self.set_control(CID_GOP_SIZE, i32::MAX as i64, "GOP size")?;
        }
        self.set_control(CID_VBV_SIZE, vbv, "VBV size")?;

        let mut request = RequestBuffers {
            count: OUTPUT_BUFFERS as u32,
            type_: V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE,
            memory: V4L2_MEMORY_DMABUF,
            ..Default::default()
        };
        self.ioctl(VIDIOC_REQBUFS, &mut request, "REQBUFS output")?;

        self.allocate_surfaces(rgba)?;

        let mut request = RequestBuffers {
            count: CAPTURE_BUFFERS as u32,
            type_: V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
            memory: V4L2_MEMORY_MMAP,
            ..Default::default()
        };
        self.ioctl(VIDIOC_REQBUFS, &mut request, "REQBUFS capture")?;
        for index in 0..CAPTURE_BUFFERS {
            let mut planes = [Plane::default(); 1];
            let mut buffer = self.buffer(
                V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
                V4L2_MEMORY_MMAP,
                index as u32,
                planes.as_mut_ptr(),
                1,
            );
            self.ioctl(VIDIOC_QUERYBUF, &mut buffer, "QUERYBUF capture")?;
            let length = planes[0].length as usize;
            let offset = planes[0].m as i64;
            let mut export = ExportBuffer {
                type_: V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
                index: index as u32,
                plane: 0,
                ..Default::default()
            };
            self.ioctl(VIDIOC_EXPBUF, &mut export, "EXPBUF capture")?;
            // The vendor's own NvBuffer::map maps this fd, not the encoder's: mapping the encoder
            // fd answers ENODEV on this node.
            let data = unsafe {
                libc::mmap(
                    ptr::null_mut(),
                    length,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    export.fd,
                    offset,
                )
            };
            unsafe { libc::close(export.fd) };
            if data == libc::MAP_FAILED {
                return Err(format!(
                    "mmap of a capture plane failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            self.capture[index] = (data, length);
            self.ioctl(VIDIOC_QBUF, &mut buffer, "QBUF capture")?;
        }

        let mut type_ = V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE;
        self.ioctl(VIDIOC_STREAMON, &mut type_, "STREAMON output")?;
        let mut type_ = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
        self.ioctl(VIDIOC_STREAMON, &mut type_, "STREAMON capture")?;
        Ok(())
    }

    fn buffer(
        &self,
        type_: u32,
        memory: u32,
        index: u32,
        planes: *mut Plane,
        count: u32,
    ) -> Buffer {
        Buffer {
            index,
            type_,
            bytesused: 0,
            flags: 0,
            field: 0,
            _pad: 0,
            timestamp: [0; 2],
            timecode: [0; 4],
            sequence: 0,
            memory,
            m: planes as u64,
            length: count,
            reserved2: 0,
            reserved: 0,
            _tail: 0,
        }
    }

    pub fn codec(&self) -> Codec {
        self.codec
    }

    /// The wire picture type of one access unit, read with the codec's own syntax: an H.265
    /// NAL header is two bytes where H.264's is one, so the wrong reader labels every IDR a
    /// delta and no client ever finds an entry point.
    fn frame_type(&self, bytes: &[u8]) -> u8 {
        match self.codec {
            Codec::H265 => h265_frame_type(bytes),
            Codec::Av1 => frame_type_from_key(av1_is_key(bytes)),
            _ => h264_frame_type(bytes),
        }
    }

    /// The VIC converts into NV12 and the encoder takes nothing else.
    pub fn is_fullcolor(&self) -> bool {
        false
    }

    /// The NV12 the VIC converts into is limited range.
    pub fn is_full_range(&self) -> bool {
        false
    }

    /// Apply a live bitrate change. The control is writable while streaming on this path, unlike
    /// the same change through the vendor GStreamer element, which the driver accepts and ignores.
    pub fn reconfigure_rate(&mut self, settings: &CaptureSettings) -> Result<(), String> {
        let wanted = (settings.video_bitrate_kbps.max(1) as u32).saturating_mul(1000);
        let fps = settings.target_fps.max(1.0);
        // A changed frame rate goes to the driver too: leaving it behind would budget the new
        // bitrate against the old rate.
        if (fps - self.fps).abs() > 0.01 {
            self.set_frame_rate(fps)?;
        }
        if wanted == self.bitrate_bps {
            return Ok(());
        }
        self.set_control(CID_BITRATE, wanted as i64, "bitrate")?;
        self.set_control(
            CID_VBV_SIZE,
            (wanted as f64 / settings.target_fps.max(1.0)) as i64,
            "VBV size",
        )?;
        self.bitrate_bps = wanted;
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
        let height = self.height as usize;
        let needed = stride * (height - 1) + self.row_bytes;
        if stride < self.row_bytes || pixels.len() < needed {
            return Err("input buffer too small".into());
        }
        self.fill_staging(pixels, stride)?;
        self.submit(frame_number, force_idr, false)
    }

    /// Bring the newest frame's access unit out on a still screen. With no next frame coming the
    /// unit would stay inside, so the frame is queued again from its staging copy, under the
    /// number of the tick that repeats it; the repeat's own unit is one more of the same picture.
    pub fn push_held(&mut self, frame_number: u64) -> Result<Vec<u8>, String> {
        if !self.holds_frame() {
            return Ok(Vec::new());
        }
        self.submit(frame_number, false, true)
    }

    /// Whether a frame's unit is inside with room to repeat it.
    pub fn holds_frame(&self) -> bool {
        self.held.is_some() && self.outstanding < OUTPUT_BUFFERS - 1
    }

    /// Queue the staging frame and take what the engine has ready, waiting for it where a unit
    /// is expected. A wait that runs out raises the number in flight one is expected from, so a
    /// deeper engine costs one wait per depth rather than one per frame.
    fn submit(
        &mut self,
        frame_number: u64,
        force_idr: bool,
        repeat: bool,
    ) -> Result<Vec<u8>, String> {
        let slot = if self.queued < OUTPUT_BUFFERS {
            self.queued
        } else {
            self.reclaim_output()?
        };
        self.convert_to_nv12(slot)?;

        if let Some(layout) = self.rps {
            self.queue_references(layout, slot, frame_number, force_idr)?;
        } else if force_idr {
            self.set_control(CID_FORCE_IDR_FRAME, 1, "force IDR")?;
        }
        let mut planes = [Plane::default(); 2];
        planes[0].m = self.nv12_fd[slot] as u64;
        planes[1].m = self.nv12_fd[slot] as u64;
        planes[0].bytesused = (self.width * self.height) as u32;
        planes[1].bytesused = (self.width * self.height / 2) as u32;
        let mut buffer = self.buffer(
            V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE,
            V4L2_MEMORY_DMABUF,
            slot as u32,
            planes.as_mut_ptr(),
            2,
        );
        buffer.flags |= V4L2_BUF_FLAG_TIMESTAMP_COPY;
        buffer.timestamp = [frame_number as i64, 0];
        // The references were stored under the slot's index (`config_store`), and the buffer
        // names that store in `reserved2`, as the vendor's own encoder does. Left zero, every
        // frame reads store 0, and the first one queued elsewhere fails the session.
        if self.rps.is_some() {
            buffer.reserved2 = slot as u32;
        }
        self.ioctl(VIDIOC_QBUF, &mut buffer, "QBUF output")?;
        self.queued = (self.queued + 1).min(OUTPUT_BUFFERS);
        self.outstanding += 1;
        if !repeat {
            self.held = Some((frame_number, slot));
        }
        if self.outstanding >= self.wait_from {
            let mut poll = libc::pollfd {
                fd: self.fd,
                events: libc::POLLIN,
                revents: 0,
            };
            if unsafe { libc::poll(&mut poll, 1, OUTPUT_WAIT_MS) } != 1 {
                self.wait_from = self.outstanding + 1;
            }
        }
        self.collect()
    }

    /// Wait for one queued output buffer to come back, which frees its slot. The node is
    /// non-blocking, so this is the one place that waits, and only when all of them are in flight.
    fn reclaim_output(&mut self) -> Result<usize, String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        loop {
            let mut planes = [Plane::default(); 2];
            let mut buffer = self.buffer(
                V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE,
                V4L2_MEMORY_DMABUF,
                0,
                planes.as_mut_ptr(),
                2,
            );
            let rc = unsafe {
                (self.vendor.ioctl)(
                    self.fd,
                    VIDIOC_DQBUF,
                    &mut buffer as *mut Buffer as *mut c_void,
                )
            };
            if rc >= 0 {
                return Ok(buffer.index as usize);
            }
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EAGAIN) {
                return Err(format!("DQBUF output failed: {err}"));
            }
            if std::time::Instant::now() > deadline {
                return Err("the encoder did not return an output buffer in 500 ms".into());
            }
            let mut poll = libc::pollfd {
                fd: self.fd,
                events: libc::POLLOUT,
                revents: 0,
            };
            unsafe { libc::poll(&mut poll, 1, 20) };
        }
    }

    /// Take the access units the encoder has ready.
    fn collect(&mut self) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        self.last_reference = Reference::Untracked;
        self.units.clear();
        loop {
            let mut planes = [Plane::default(); 1];
            let mut buffer = self.buffer(
                V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
                V4L2_MEMORY_MMAP,
                0,
                planes.as_mut_ptr(),
                1,
            );
            let rc = unsafe {
                (self.vendor.ioctl)(
                    self.fd,
                    VIDIOC_DQBUF,
                    &mut buffer as *mut Buffer as *mut c_void,
                )
            };
            if rc < 0 {
                // EAGAIN is the only answer that means "nothing ready yet" on a non-blocking
                // node. Anything else is a session that has stopped working, and reporting it as
                // an empty frame would leave the caller waiting on a stream that never returns
                // instead of rebuilding the encoder.
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EAGAIN) {
                    break;
                }
                return Err(format!("DQBUF capture failed: {err}"));
            }
            let index = buffer.index as usize;
            let length = planes[0].bytesused as usize;
            self.outstanding = self.outstanding.saturating_sub(1);
            if self
                .held
                .is_some_and(|(number, _)| number == buffer.timestamp[0] as u64)
            {
                self.held = None;
            }
            if length > 0 {
                let (data, _) = self.capture[index];
                let unit = unsafe { std::slice::from_raw_parts(data as *const u8, length) };
                let number = buffer.timestamp[0] as u64;
                let reference = self.reference_of(number);
                let frame_type = self.frame_type(unit);
                if frame_type == FRAME_KEY {
                    self.note_key_frame(reference);
                }
                let bounded = (frame_type == FRAME_KEY && self.codec == Codec::H264)
                    .then(|| self.reorder.apply(unit))
                    .flatten();
                let bytes = bounded.as_deref().unwrap_or(unit);
                if self.units.is_empty() {
                    self.last_reference = reference;
                }
                if self.omit_headers {
                    out.extend_from_slice(bytes);
                } else {
                    out.reserve(VIDEO_HEADER_LEN + bytes.len());
                    push_video_header(
                        &mut out,
                        self.codec,
                        frame_type,
                        number as u16,
                        0,
                        self.width as u16,
                        self.height as u16,
                        reference,
                    );
                    out.extend_from_slice(bytes);
                }
                self.units.push((number as u16, reference, out.len()));
            }
            self.ioctl(VIDIOC_QBUF, &mut buffer, "QBUF capture")?;
        }
        Ok(out)
    }
}

/// The session is moved onto the encode thread and used from one thread at a time. What makes it
/// not `Send` on its own are the capture-plane mappings it owns and frees itself, as with the
/// NVENC and VA-API sessions next door.
unsafe impl Send for TegraEncoder {}

impl Drop for TegraEncoder {
    fn drop(&mut self) {
        let mut type_ = V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE;
        let _ = self.ioctl(VIDIOC_STREAMOFF, &mut type_, "STREAMOFF output");
        let mut type_ = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
        let _ = self.ioctl(VIDIOC_STREAMOFF, &mut type_, "STREAMOFF capture");
        for (data, length) in self.capture {
            if !data.is_null() {
                unsafe { libc::munmap(data, length) };
            }
        }
        unsafe {
            (self.vendor.close)(self.fd);
            match self.surfaces {
                Surfaces::Utils(api) => {
                    for fd in self.nv12_fd {
                        if fd >= 0 {
                            (api.destroy)(fd);
                        }
                    }
                    if self.staging_fd >= 0 {
                        (api.destroy)(self.staging_fd);
                    }
                }
                Surfaces::Surface(api) => {
                    if !self.staging_surf.is_null() {
                        if !self.staging_map.is_null() {
                            (api.unmap)(self.staging_surf, 0, 0);
                        }
                        (api.destroy)(self.staging_surf);
                    }
                    for surf in self.nv12_surf {
                        if !surf.is_null() {
                            (api.destroy)(surf);
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The capture format is the vendor library's spelling, which is not the one a mainline V4L2
    /// encoder uses: NVIDIA's header defines HEVC as `H265` where the kernel defines `HEVC`, and
    /// a session that sent the kernel's fourcc would be refused a format the device does serve.
    #[test]
    fn hevc_is_spelled_the_way_the_vendor_library_spells_it() {
        assert_eq!(
            coded_fourcc(Codec::H264),
            Some(u32::from_le_bytes(*b"H264"))
        );
        assert_eq!(
            coded_fourcc(Codec::H265),
            Some(u32::from_le_bytes(*b"H265"))
        );
        assert_eq!(coded_fourcc(Codec::Av1), Some(u32::from_le_bytes(*b"AV10")));
        assert_ne!(
            coded_fourcc(Codec::H265),
            Some(u32::from_le_bytes(*b"HEVC")),
            "the kernel's HEVC fourcc is not the vendor library's"
        );
        for codec in [Codec::Vp8, Codec::Vp9, Codec::Jpeg] {
            assert_eq!(
                coded_fourcc(codec),
                None,
                "{} has no vendor encoder here",
                codec.display()
            );
        }
    }

    /// Every codec the backend reports is one it can open, so the ladder never picks a hardware
    /// path that the session then refuses and logs as a failure: AV1 is withheld on the
    /// generations without the engine, whose node enumerates the format regardless, and kept
    /// on an Orin and on any board this does not know.
    #[test]
    fn every_codec_reported_is_one_a_session_can_be_opened_for() {
        let nano = b"nvidia,p3449-0000-b00+p3448-0000-b00\0nvidia,jetson-nano\0nvidia,tegra210\0";
        let tx2 = b"nvidia,quill\0nvidia,tegra186\0";
        let xavier =
            b"nvidia,galen\0nvidia,jetson-xavier\0nvidia,p2822-0000+p2888-0001\0nvidia,tegra194\0";
        let orin = b"nvidia,p3737-0000+p3701-0000\0nvidia,p3701-0000\0nvidia,tegra234\0";
        let thor = b"nvidia,tegra264\0";
        for (board, av1) in [
            (&nano[..], false),
            (tx2, false),
            (xavier, false),
            (orin, true),
            (thor, true),
            (b"", true),
        ] {
            let served = served_on(board);
            assert_eq!(
                served.contains(&Codec::Av1),
                av1,
                "{}",
                String::from_utf8_lossy(board)
            );
            assert!(served.contains(&Codec::H264) && served.contains(&Codec::H265));
            for codec in &served {
                assert!(
                    coded_fourcc(*codec).is_some(),
                    "{} is reported but has no format",
                    codec.display()
                );
            }
        }
        assert!(!served().is_empty());
    }

    /// The profile control is the codec's own: H.264's is a standard kernel CID and H.265's a
    /// vendor extension, and setting one on the other codec is a refusal that costs the session
    /// its hardware encoder.
    #[test]
    fn the_profile_control_is_the_codec_s_own() {
        assert_ne!(CID_H264_PROFILE, CID_H265_PROFILE);
        assert_eq!(
            CID_H265_PROFILE,
            0x0099_0900 + 513,
            "V4L2_CID_MPEG_BASE + 513"
        );
        assert_eq!(
            CID_H264_PROFILE,
            0x0099_0900 + 363,
            "V4L2_CID_MPEG_VIDEO_H264_PROFILE"
        );
    }

    /// The external-RPS layout follows the release the host runs, and only the two measured:
    /// JetPack 5 lays the structures out in a way no board here has shown.
    #[test]
    fn the_reference_set_is_laid_out_for_the_release_measured() {
        let orin = "# R36 (release), REVISION: 4.3, GCID: 38968081, BOARD: generic, EABI: aarch64";
        let nano = "# R32 (release), REVISION: 6.1, GCID: 27863751, BOARD: t210ref, EABI: aarch64";
        assert_eq!(rps_layout_for(orin).map(|l| l.metadata), Some(72));
        assert_eq!(rps_layout_for(nano).map(|l| l.metadata), Some(56));
        assert!(
            rps_layout_for("# R35 (release), REVISION: 6.0").is_none(),
            "JetPack 5 was never measured"
        );
        assert!(rps_layout_for("").is_none());
        assert_eq!(CID_INPUT_METADATA, 0x0099_0900 + 541);
        assert_eq!(CID_EXTERNAL_RPS, 0x0099_0900 + 542);
        assert_eq!(CID_NUM_REFERENCE_FRAMES, 0x0099_0900 + 532);
        for layout in [&RPS_R32, &RPS_R36] {
            assert!(
                20 + RPS_LIST_LEN * layout.prop <= layout.params,
                "the list fits its structure"
            );
            assert!(layout.config_store + 4 <= layout.metadata);
            assert!(
                layout.config_store >= 40,
                "past the pointers the metadata leads with"
            );
            assert!(
                8 + 4 <= layout.enable,
                "both counts fit the enable structure"
            );
        }
    }

    /// Test helper: a `w×h` BGRA frame of hashed noise with a block moved `step` places along
    /// its top rows, so every frame predicts from the one before and differs from it.
    fn moving(w: usize, h: usize, step: u64) -> Vec<u8> {
        let mut f = vec![0u8; w * h * 4];
        for (i, px) in f.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let v = ((i as u32).wrapping_mul(2654435761) >> 24) as u8;
            *px = [v, v ^ 10, 10, 255];
        }
        let x0 = (step as usize * 64) % (w - 256);
        for row in 0..256.min(h) {
            let dst = (row * w + x0) * 4;
            f[dst..dst + 256 * 4].fill(200);
        }
        f
    }

    /// Test helper: a session with the unit headers left off, so each unit decodes as it comes;
    /// None where this host has no Tegra encoder.
    fn session(codec: Codec, w: usize, h: usize, keyframe_s: f64) -> Option<TegraEncoder> {
        let s = CaptureSettings {
            width: w as i32,
            height: h as i32,
            codec,
            target_fps: 30.0,
            keyframe_interval_s: keyframe_s,
            omit_stripe_headers: true,
            ..Default::default()
        };
        match TegraEncoder::new(codec, &s, false) {
            Ok(enc) => Some(enc),
            Err(e) => {
                println!("no Tegra {codec:?} session: {e}");
                None
            }
        }
    }

    /// Test helper: encode frame `i` and wait for its own unit, so each call answers for that
    /// frame alone although the encoder hands units back a frame or two late.
    fn encode_one(
        enc: &mut TegraEncoder,
        i: u64,
        w: usize,
        h: usize,
        key: bool,
    ) -> (Vec<u8>, Reference) {
        encode_frame(enc, &moving(w, h, i), w, i, key)
    }

    /// Test helper: `encode_one` for a frame of the caller's. Waits by the units handed back,
    /// which a session names whether or not it names references.
    fn encode_frame(
        enc: &mut TegraEncoder,
        pixels: &[u8],
        w: usize,
        i: u64,
        key: bool,
    ) -> (Vec<u8>, Reference) {
        let mut out = enc
            .encode_host(pixels, w * 4, false, i, 25, key)
            .expect("encode");
        let mut came = enc.units.iter().any(|u| u.0 == i as u16);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !came {
            assert!(
                std::time::Instant::now() < deadline,
                "frame {i} never came back"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
            out.extend(enc.collect().expect("collect"));
            came = enc.units.iter().any(|u| u.0 == i as u16);
        }
        (out, enc.last_reference())
    }

    /// Test helper: whether a unit carries the parameter sets a client starts on — the SPS, and
    /// for H.265 the VPS ahead of it.
    fn carries_parameter_sets(codec: Codec, unit: &[u8]) -> bool {
        match codec {
            Codec::H265 => {
                let types: Vec<u8> = unit
                    .windows(4)
                    .filter(|w| w[..3] == [0, 0, 1])
                    .map(|w| (w[3] >> 1) & 0x3f)
                    .collect();
                types.contains(&32) && types.contains(&33)
            }
            _ => crate::encoders::sps::h264_frame_num_range(unit).is_some(),
        }
    }

    /// A frame a client lost is left out of the encoder's predictions: the next frame predicts
    /// from the newest frame before it and names it, and a decoder that never saw the lost
    /// frames shows the frames after them exactly as one that saw everything. A key frame the
    /// window asks for carries the parameter sets a client starts on. Ignored by default.
    #[test]
    #[ignore]
    fn tegra_predicts_past_a_lost_frame() {
        use crate::webcam::decode::{Decoder as _, VideoDecoder};
        let (w, h) = (1280usize, 720usize);
        for codec in [Codec::H264, Codec::H265] {
            let Some(mut enc) = session(codec, w, h, 10.0) else {
                continue;
            };
            let (first, reference) = encode_one(&mut enc, 0, w, h, true);
            if reference == Reference::Untracked {
                println!("{codec:?}: this encoder takes no reference set, so nothing is tracked");
                assert!(!enc.invalidate_reference(0));
                continue;
            }
            assert_eq!(reference, Reference::None);
            let mut frames = vec![first];
            for i in 1..8u64 {
                let (out, reference) = encode_one(&mut enc, i, w, h, false);
                assert_eq!(
                    reference,
                    Reference::Frame(i as u16 - 1),
                    "{codec:?} frame {i}"
                );
                frames.push(out);
            }
            assert!(enc.invalidate_reference(5));
            let (out, reference) = encode_one(&mut enc, 8, w, h, false);
            assert_eq!(
                reference,
                Reference::Frame(4),
                "{codec:?}: frame 8 predicts past the lost 5-7"
            );
            frames.push(out);
            let (out, reference) = encode_one(&mut enc, 9, w, h, false);
            assert_eq!(reference, Reference::Frame(8), "{codec:?}");
            frames.push(out);

            let (mut whole, mut lossy) = (
                VideoDecoder::new(codec).unwrap(),
                VideoDecoder::new(codec).unwrap(),
            );
            for (i, f) in frames.iter().enumerate() {
                assert!(whole.decode(f).expect("decode"), "{codec:?} frame {i}");
                if !(5..8).contains(&i) {
                    assert!(
                        lossy.decode(f).expect("decode without 5-7"),
                        "{codec:?} frame {i}"
                    );
                }
            }
            let (a, b) = (whole.frame().unwrap(), lossy.frame().unwrap());
            let differ =
                a.y.chunks(a.y_stride)
                    .zip(b.y.chunks(b.y_stride))
                    .take(a.height)
                    .filter(|(ra, rb)| ra[..a.width] != rb[..a.width])
                    .count();
            assert_eq!(
                differ, 0,
                "{codec:?}: frame 9 without frames 5-7 differs from the complete decode in {differ} rows"
            );

            // Frame 1 has left the window, and everything held predicts through it: a key frame,
            // asked for with an empty set, which a client that saw nothing before it starts on.
            let mut i = 10u64;
            for _ in 0..enc.dpb + 2 {
                encode_one(&mut enc, i, w, h, false);
                i += 1;
            }
            assert!(enc.invalidate_reference(1));
            let (key, reference) = encode_one(&mut enc, i, w, h, false);
            assert_eq!(
                reference,
                Reference::None,
                "{codec:?}: a loss out of the window costs a key frame"
            );
            assert_eq!(enc.frame_type(&key), FRAME_KEY, "{codec:?}");
            assert!(
                carries_parameter_sets(codec, &key),
                "{codec:?}: the key frame the session asked for carries its parameter sets"
            );
            let mut fresh = VideoDecoder::new(codec).unwrap();
            assert!(
                fresh.decode(&key).expect("decode"),
                "{codec:?}: a client starts on it alone"
            );
            assert_eq!(
                encode_one(&mut enc, i + 1, w, h, false).1,
                Reference::Frame(i as u16),
                "{codec:?}"
            );
            assert!(
                !enc.resync,
                "{codec:?}: the encoder coded no key frame the session did not ask for"
            );
        }
    }

    /// With the reference set the session's, so is every key frame: the periodic one comes when
    /// the interval says, at no other frame, and the encoder inserts none of its own. Ignored by
    /// default.
    #[test]
    #[ignore]
    fn tegra_codes_the_key_frames_the_session_asks_for_and_no_other() {
        let (w, h) = (1280usize, 720usize);
        for codec in [Codec::H264, Codec::H265] {
            let Some(mut enc) = session(codec, w, h, 1.0) else {
                continue;
            };
            if encode_one(&mut enc, 0, w, h, true).1 == Reference::Untracked {
                println!("{codec:?}: this encoder takes no reference set, so nothing is tracked");
                continue;
            }
            for i in 1..95u64 {
                let (out, reference) = encode_one(&mut enc, i, w, h, false);
                let key = enc.frame_type(&out) == FRAME_KEY;
                assert_eq!(
                    key,
                    i % 30 == 0,
                    "{codec:?} frame {i}: a key frame every second at 30 fps, and only then"
                );
                assert_eq!(
                    reference == Reference::None,
                    key,
                    "{codec:?} frame {i}: the header says what the bitstream is"
                );
                assert!(
                    !enc.resync,
                    "{codec:?} frame {i}: the encoder coded a key frame nobody asked for"
                );
            }
            // Asked for between two periodic ones, and the count restarts there.
            let (out, reference) = encode_one(&mut enc, 95, w, h, true);
            assert_eq!(
                (enc.frame_type(&out), reference),
                (FRAME_KEY, Reference::None),
                "{codec:?}"
            );
            for i in 96..125u64 {
                assert_ne!(
                    encode_one(&mut enc, i, w, h, false).1,
                    Reference::None,
                    "{codec:?} frame {i}"
                );
            }
            assert_eq!(
                encode_one(&mut enc, 125, w, h, false).1,
                Reference::None,
                "{codec:?}: thirty frames after the one asked for"
            );
        }
    }

    /// Test helper: a flat blue `w×h` BGRA frame with a red bar 160 wide moved 12 pixels a step,
    /// the motion a desktop has: slow enough that every frame predicts the bar from the last.
    fn bar_scene(w: usize, h: usize, step: u64) -> (Vec<u8>, usize) {
        let mut f = [0x78u8, 0x28, 0x1e, 255].repeat(w * h);
        let x0 = (step as usize * 12) % (w - 160);
        for row in 0..h {
            for px in f[(row * w + x0) * 4..(row * w + x0 + 160) * 4].chunks_mut(4) {
                px.copy_from_slice(&[0x28, 0x3c, 0xdc, 255]);
            }
        }
        (f, x0 + 80)
    }

    /// Every frame decodes to what was encoded, not only to what another decoder makes of it:
    /// a frame predicted from a reference other than the one its bitstream names drifts where
    /// the picture moves, and comparing two decodes of one stream cannot see that, since both
    /// drift alike. The bar's luma is held to the key frame's across a loss. Ignored by default.
    #[test]
    #[ignore]
    fn tegra_decodes_what_it_encoded() {
        use crate::webcam::decode::{Decoder as _, VideoDecoder};
        let (w, h) = (1280usize, 720usize);
        for codec in [Codec::H264, Codec::H265] {
            let Some(mut enc) = session(codec, w, h, 10.0) else {
                continue;
            };
            if enc.references.is_none() {
                println!(
                    "{codec:?}: this session takes no reference set, so a loss costs a key frame"
                );
                continue;
            }
            let mut dec = VideoDecoder::new(codec).unwrap();
            let mut first = None;
            let mut worst = 0u8;
            for i in 0..90u64 {
                // Frame 39 is lost, and the client's report reaches the session before frame 41.
                if i == 41 {
                    assert!(enc.invalidate_reference(39), "{codec:?}");
                }
                let (pixels, bar) = bar_scene(w, h, i);
                let (out, _) = encode_frame(&mut enc, &pixels, w, i, i == 0);
                // 39 is lost by the client, and 40, which predicts from it, is held back.
                if i == 39 || i == 40 {
                    continue;
                }
                assert!(dec.decode(&out).expect("decode"), "{codec:?} frame {i}");
                let f = dec.frame().unwrap();
                let luma = f.y[(h / 2) * f.y_stride + bar];
                if std::env::var_os("TEGRA_TRACE").is_some() {
                    print!("{i}:{luma} ");
                }
                let first = *first.get_or_insert(luma);
                worst = worst.max(luma.abs_diff(first));
                assert!(
                    luma.abs_diff(first) <= 6,
                    "{codec:?} frame {i}: the bar reads {luma}, the key frame {first}"
                );
            }
            println!("{codec:?}: the bar held within {worst} of the key frame's luma");
        }
    }

    /// Content that repeats a few frames apart lets motion search reach past the newest
    /// reference, and a picture predicted from a reference its decoder resolves differently
    /// drifts from the source there. A bar moving over a strip that cycles through four noise
    /// images is held to the source's luma, with the session's reference set where it names one
    /// and without it for H.264, whose external set the vendor encoder mis-codes (19.9 off the
    /// source where this holds it under 8). Ignored by default.
    #[test]
    #[ignore]
    fn tegra_decodes_repeating_content_as_encoded() {
        use crate::webcam::decode::{Decoder as _, VideoDecoder};
        let (w, h) = (1280usize, 720usize);
        let noise: Vec<Vec<u8>> = (0..4u32)
            .map(|k| {
                (0..w * 240)
                    .map(|i| {
                        ((i as u32).wrapping_mul(2654435761).wrapping_add(k * 77777) >> 24) as u8
                    })
                    .collect()
            })
            .collect();
        for codec in [Codec::H264, Codec::H265] {
            let Some(mut enc) = session(codec, w, h, 10.0) else {
                continue;
            };
            let mut dec = VideoDecoder::new(codec).unwrap();
            let mut worst = 0f64;
            for i in 0..60u64 {
                let (mut pixels, _) = bar_scene(w, h, i);
                let strip = &noise[i as usize % 4];
                for row in 0..240 {
                    for x in 0..w {
                        let v = strip[row * w + x];
                        let at = ((h - 240 + row) * w + x) * 4;
                        pixels[at..at + 3].copy_from_slice(&[v, v, v]);
                    }
                }
                let (out, _) = encode_frame(&mut enc, &pixels, w, i, i == 0);
                assert!(dec.decode(&out).expect("decode"), "{codec:?} frame {i}");
                if i < 8 {
                    continue;
                }
                // Gray v is luma 16 + 219 v / 255 in the limited range the session declares; the
                // rows at the strip's edges are left to the quantizer.
                let f = dec.frame().unwrap();
                let mut off = 0f64;
                for row in 8..232 {
                    let decoded = &f.y[(h - 240 + row) * f.y_stride..][..w];
                    for (x, &y) in decoded.iter().enumerate() {
                        off +=
                            (y as f64 - (16.0 + 219.0 * strip[row * w + x] as f64 / 255.0)).abs();
                    }
                }
                worst = worst.max(off / (224 * w) as f64);
            }
            println!("{codec:?}: the repeating strip held within {worst:.2} of the source's luma");
            assert!(
                worst < 8.0,
                "{codec:?}: the repeating strip drifted {worst:.2} from the source"
            );
        }
    }
}
