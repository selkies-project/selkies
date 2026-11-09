/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! NVENC hardware H.264 / HEVC / AV1 encoder: CUDA-bound sessions that encode packed BGRA or
//! RGBA frames from the X11 host and Wayland readback paths, or Wayland dmabufs in place.
//!
//! The module dynamically loads `libcuda`, `libnvidia-encode`, and `libEGL` at runtime, negotiates
//! the NVENC API version against the installed driver (set-once per process), and stamps every
//! NVENCAPI struct with the exact `NV_ENC_*_VER` word the negotiated SDK defines, so one binary
//! drives drivers from NVENC 10.0 (~R445) through 13.0. Frames reach the GPU two ways: a
//! zero-copy dmabuf import (EGLImage → CUDA, the mapped plane registered with NVENC in place as
//! pitch-linear memory or as a CUDA array), and a pinned host→device upload of packed BGRA / RGBA.
//! Color is converted on the GPU either way. The codec is a session parameter: the same rate
//! control, GOP, VUI, and latency posture is programmed into whichever of the three codec
//! configurations the device offers, and a codec the device lacks (AV1 before Ada) is refused at
//! open so the caller falls back. Sessions reconfigure resolution and rate control in place, so a
//! resize or bitrate change costs a few milliseconds instead of a full rebuild.

// The NVENC and CUDA entry points are called through function pointers
// resolved at runtime, so the safety contract is carried by the function
// signatures rather than by a block around each call.
#![allow(unsafe_op_in_unsafe_fn)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

use std::collections::{HashMap, VecDeque};
use std::ffi::{CStr, CString, c_char, c_void};
use std::os::unix::io::AsRawFd;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use libloading::{Library, Symbol};
use smithay::backend::allocator::{Buffer, Fourcc, dmabuf::Dmabuf};

use super::codec::{
    Codec, FRAME_ANCHOR, FRAME_DELTA, FRAME_INTRA, FRAME_KEY, Hardware, VIDEO_HEADER_LEN,
    av1_level, h264_dpb_frames, h264_level, h265_dpb_frames, h265_level, h265_tier,
    push_video_header,
};
use super::dmabuf_identity::DmaBufIdentity;
use super::frame_rate::FrameRate;
use super::reference::{ANCHORS, Invalidation, Reference, ReferenceReport, ReferenceWindow};
use super::session::{PeakBucket, RateSettings};
use super::sps::{WideFrameNum, h264_frame_num_range};
use crate::CaptureSettings;
use nvcodec_sys::cuda::*;
use nvcodec_sys::*;

/// Opaque CUDA module and kernel handles; the driver API's own types, which the committed
/// bindings do not carry.
type CUmodule = *mut c_void;
type CUfunction = *mut c_void;

/// The ARGB/ABGR → NV12 convert and the band hash, as PTX the driver JIT-compiles at session
/// open. PTX is the portable form: `libcuda` compiles it for whatever GPU is present, so nothing
/// beyond the driver NVENC already needs has to be installed, and `.version 3.1`/`.target sm_30`
/// keeps every NVENC-capable GPU in range.
const ARGB_TO_NV12_PTX: &[u8] = include_bytes!("argb_to_nv12.ptx");

/// JIT the kernels' module into the current context; `None` where the driver refuses it.
unsafe fn load_kernels(cuda: &CudaFunctions) -> Option<CUmodule> {
    let mut ptx = ARGB_TO_NV12_PTX.to_vec();
    ptx.push(0);
    let mut module: CUmodule = ptr::null_mut();
    ((cuda.cuModuleLoadData)(&mut module, ptx.as_ptr() as *const c_void) == CUresult::CUDA_SUCCESS)
        .then_some(module)
}

/// EGL C-interop type aliases and the `EGL_*` attribute constants used to wrap a dmabuf as
/// an `EGLImageKHR` for CUDA import.
type EGLDisplay = *const c_void;
type EGLImageKHR = *mut c_void;
type EGLint = i32;
type EGLenum = u32;
type EGLBoolean = u32;

const EGL_NO_IMAGE_KHR: EGLImageKHR = ptr::null_mut();
const EGL_LINUX_DMA_BUF_EXT: u32 = 0x3270;
const EGL_DMA_BUF_PLANE0_FD_EXT: EGLint = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: EGLint = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: EGLint = 0x3274;
const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: EGLint = 0x3443;
const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: EGLint = 0x3444;
const EGL_WIDTH: EGLint = 0x3057;
const EGL_HEIGHT: EGLint = 0x3056;
const EGL_LINUX_DRM_FOURCC_EXT: EGLint = 0x3271;
const EGL_NONE: EGLint = 0x3038;

/// Opaque CUDA graphics-resource handle for the EGL interop path — an `EGLImageKHR`
/// registered with CUDA maps to one of these.
type CUgraphicsResource = *mut c_void;

/// `CUeglFrame::frame_type` values: the mapped planes are CUDA arrays, or pitch-linear device
/// memory.
const CU_EGL_FRAME_TYPE_ARRAY: u32 = 0;
const CU_EGL_FRAME_TYPE_PITCH: u32 = 1;
/// `CUeglFrame::cu_format` of an 8-bit-per-channel plane (`CU_AD_FORMAT_UNSIGNED_INT8`).
const CU_AD_FORMAT_U8: u32 = 1;

/// `CU_TRSF_READ_AS_INTEGER`, a `cuda.h` macro the bindings do not carry: a texture fetch
/// returns the stored bytes rather than normalized floats.
const CU_TRSF_READ_AS_INTEGER: u32 = 1;

/// A CUDA frame mapped from an EGLImage: the `cuGraphicsResourceGetMappedEglFrame` result
/// describing the imported dmabuf's plane pointers, geometry, pitch, and pixel format.
#[repr(C)]
#[derive(Clone, Copy)]
struct CUeglFrame {
    frame: CUeglFrameUnion,
    width: u32,
    height: u32,
    depth: u32,
    pitch: u32,
    plane_count: u32,
    num_channels: u32,
    frame_type: u32,
    egl_color_format: u32,
    cu_format: u32,
}

/// The mapped frame's plane pointers, as either CUDA arrays or pitch-linear device
/// pointers — `CUeglFrame::frame_type` selects which arm of the union is valid.
#[repr(C)]
#[derive(Clone, Copy)]
union CUeglFrameUnion {
    p_array: [CUarray; 3],
    p_pitch: [*mut c_void; 3],
}

type EglCreateImageKhrFn = unsafe extern "C" fn(
    dpy: EGLDisplay,
    ctx: *mut c_void,
    target: EGLenum,
    buffer: *mut c_void,
    attrib_list: *const EGLint,
) -> EGLImageKHR;
type EglDestroyImageKhrFn = unsafe extern "C" fn(dpy: EGLDisplay, image: EGLImageKHR) -> EGLBoolean;

/// Dynamically loaded EGL entry points (from `libEGL`) for creating and destroying the
/// `EGLImageKHR` that wraps a dmabuf. `_lib` keeps the library resident for the pointers' life.
struct EglFunctions {
    _lib: Library,
    eglGetProcAddress: unsafe extern "C" fn(procname: *const c_char) -> *mut c_void,
    eglCreateImageKHR: EglCreateImageKhrFn,
    eglDestroyImageKHR: EglDestroyImageKhrFn,
}

/// Dynamically loaded CUDA driver-API entry points (from `libcuda`) for context, device,
/// memory, host-pin, and EGL-interop calls. `_lib` keeps the library resident for the pointers' life.
struct CudaFunctions {
    _lib: Library,
    cuInit: unsafe extern "C" fn(flags: u32) -> CUresult,
    cuDeviceGet: unsafe extern "C" fn(device: *mut CUdevice, ordinal: i32) -> CUresult,
    cuDeviceGetByPCIBusId:
        unsafe extern "C" fn(dev: *mut CUdevice, pciBusId: *const c_char) -> CUresult,
    cuDevicePrimaryCtxRetain: unsafe extern "C" fn(pctx: *mut CUcontext, dev: CUdevice) -> CUresult,
    cuCtxPushCurrent_v2: unsafe extern "C" fn(ctx: CUcontext) -> CUresult,
    cuCtxPopCurrent_v2: unsafe extern "C" fn(pctx: *mut CUcontext) -> CUresult,
    cuDevicePrimaryCtxRelease_v2: unsafe extern "C" fn(dev: CUdevice) -> CUresult,
    cuMemAlloc_v2: unsafe extern "C" fn(dptr: *mut CUdeviceptr, bytesize: usize) -> CUresult,
    cuMemAllocPitch_v2: unsafe extern "C" fn(
        dptr: *mut CUdeviceptr,
        pPitch: *mut usize,
        WidthInBytes: usize,
        Height: usize,
        ElementSizeBytes: u32,
    ) -> CUresult,
    cuMemFree_v2: unsafe extern "C" fn(dptr: CUdeviceptr) -> CUresult,
    cuMemcpyHtoD_v2: unsafe extern "C" fn(
        dstDevice: CUdeviceptr,
        srcHost: *const c_void,
        ByteCount: usize,
    ) -> CUresult,
    cuMemcpyDtoH_v2: unsafe extern "C" fn(
        dstHost: *mut c_void,
        srcDevice: CUdeviceptr,
        ByteCount: usize,
    ) -> CUresult,
    cuMemcpy2D_v2: unsafe extern "C" fn(pCopy: *const CUDA_MEMCPY2D) -> CUresult,
    cuMemcpy2DAsync_v2:
        unsafe extern "C" fn(pCopy: *const CUDA_MEMCPY2D, hStream: CUstream) -> CUresult,
    cuStreamSynchronize: unsafe extern "C" fn(hStream: CUstream) -> CUresult,
    cuModuleLoadData: unsafe extern "C" fn(module: *mut CUmodule, image: *const c_void) -> CUresult,
    cuModuleGetFunction: unsafe extern "C" fn(
        hfunc: *mut CUfunction,
        hmod: CUmodule,
        name: *const c_char,
    ) -> CUresult,
    cuModuleUnload: unsafe extern "C" fn(hmod: CUmodule) -> CUresult,
    cuTexObjectCreate: unsafe extern "C" fn(
        tex: *mut CUtexObject,
        res: *const CUDA_RESOURCE_DESC,
        sampling: *const CUDA_TEXTURE_DESC,
        view: *const c_void,
    ) -> CUresult,
    cuTexObjectDestroy: unsafe extern "C" fn(tex: CUtexObject) -> CUresult,
    #[allow(clippy::type_complexity)]
    cuLaunchKernel: unsafe extern "C" fn(
        f: CUfunction,
        gx: u32,
        gy: u32,
        gz: u32,
        bx: u32,
        by: u32,
        bz: u32,
        shared: u32,
        stream: CUstream,
        params: *mut *mut c_void,
        extra: *mut *mut c_void,
    ) -> CUresult,
    cuMemHostRegister_v2:
        unsafe extern "C" fn(p: *mut c_void, bytesize: usize, flags: u32) -> CUresult,
    cuMemHostUnregister: unsafe extern "C" fn(p: *mut c_void) -> CUresult,
    cuGraphicsEGLRegisterImage: unsafe extern "C" fn(
        pCudaResource: *mut CUgraphicsResource,
        image: EGLImageKHR,
        flags: u32,
    ) -> CUresult,
    cuGraphicsUnregisterResource: unsafe extern "C" fn(resource: CUgraphicsResource) -> CUresult,
    cuGraphicsResourceGetMappedEglFrame: unsafe extern "C" fn(
        pEglFrame: *mut CUeglFrame,
        resource: CUgraphicsResource,
        index: u32,
        mipLevel: u32,
    ) -> CUresult,
    cuDeviceGetCount: unsafe extern "C" fn(count: *mut i32) -> CUresult,
    cuDeviceGetName: unsafe extern "C" fn(name: *mut c_char, len: i32, dev: CUdevice) -> CUresult,
    cuDeviceGetUuid: unsafe extern "C" fn(uuid: *mut CUuuid, dev: CUdevice) -> CUresult,
    cuGetErrorName: unsafe extern "C" fn(error: CUresult, pStr: *mut *const c_char) -> CUresult,
    /// External semaphores (CUDA 10), bound only where present: the X server's blit semaphore is
    /// imported and waited on through them, and a driver without them keeps the GetImage fence.
    cuImportExternalSemaphore: Option<
        unsafe extern "C" fn(
            out: *mut CUexternalSemaphore,
            desc: *const CUDA_EXTERNAL_SEMAPHORE_HANDLE_DESC,
        ) -> CUresult,
    >,
    cuWaitExternalSemaphoresAsync: Option<
        unsafe extern "C" fn(
            semaphores: *const CUexternalSemaphore,
            params: *const CUDA_EXTERNAL_SEMAPHORE_WAIT_PARAMS,
            count: u32,
            stream: CUstream,
        ) -> CUresult,
    >,
    cuDestroyExternalSemaphore: Option<unsafe extern "C" fn(sem: CUexternalSemaphore) -> CUresult>,
}

/// Dynamically loaded NVENC entry points (from `libnvidia-encode`).
///
/// - **`create_instance`** (`NvEncodeAPICreateInstance`): fills an `NV_ENCODE_API_FUNCTION_LIST`
///   with the driver's encode entry points for a requested API-version word.
/// - **`get_max_version`** (`NvEncodeAPIGetMaxSupportedVersion`): the highest API version the
///   driver supports, used to cap version probing. `Option` because very old drivers lack it, in
///   which case probing relies on `create_instance` acceptance alone.
///
/// `_lib` keeps the library resident for the function pointers' life.
struct NvencLibrary {
    _lib: Library,
    create_instance:
        unsafe extern "C" fn(functionList: *mut NV_ENCODE_API_FUNCTION_LIST) -> NVENCSTATUS,
    get_max_version: Option<unsafe extern "C" fn(*mut u32) -> NVENCSTATUS>,
}

/// Negotiated NVENC API version `(major, minor)`, resolved once per process. `None` until
/// `nvenc_negotiate` runs; every struct-version word and the session `apiVersion` derive from it.
static NVENC_NEG_VER: std::sync::OnceLock<(u32, u32)> = std::sync::OnceLock::new();

/// The NVENCAPI structs this encoder must stamp with a per-SDK version word — enumerated
/// here precisely because getting that word exactly right is the whole mechanism that lets one
/// compiled binary satisfy every driver's version check.
///
/// Each NVENCAPI struct carries a `version` field that the driver validates against the exact word
/// its own SDK defined for that struct, rejecting anything else outright with
/// `NV_ENC_ERR_INVALID_VERSION`. Only two parts of that packed word move between SDKs — the struct
/// **revision** (bits 16-23) and the **`1<<31` flag** — so a session that has down-negotiated to an
/// older API cannot send the compiled 13.0 words; it must stamp each struct with precisely the word
/// that older SDK defined, while a current driver still receives its own native word. Naming the
/// structs here is what lets `NvStruct::rev` supply the per-version `(revision, flag)` and
/// `nvenc_struct_ver` assemble the word.
#[derive(Clone, Copy, Debug)]
enum NvStruct {
    FunctionList,
    OpenSessionExParams,
    Config,
    RcParams,
    PresetConfig,
    InitializeParams,
    ReconfigureParams,
    RegisterResource,
    MapInputResource,
    CreateBitstreamBuffer,
    PicParams,
    LockBitstream,
    CapsParam,
}

impl NvStruct {
    /// The `(struct revision, 1<<31 flag)` this struct uses under the SDK identified by the
    /// packed API version `api` (`(major<<4)|minor`) — the only two sub-fields that move between
    /// SDKs, and thus the entire per-version knowledge stamping a struct actually needs.
    ///
    /// The revision lands in bits 16-23 of the version word and the flag in bit 31; every other bit
    /// is fixed, which is exactly why matching just these two on `api` reproduces each SDK's word.
    /// The values are transcribed verbatim from `nvEncodeAPI.h` at the FFmpeg nv-codec-headers tags
    /// n10.0.26.2, n11.0.10.3, n11.1.5.3, n12.0.16.1, n12.1.14.0, n12.2.72.0, and n13.0.19.0, so they
    /// are ground truth rather than anything derived that could drift. Structs whose layout is stable
    /// across those SDKs return a constant pair; the rest match on `api`. 10.0 is the negotiation
    /// floor, so the oldest match arm also covers anything below it.
    fn rev(self, api: u32) -> (u32, bool) {
        match self {
            NvStruct::FunctionList => (2, false),
            NvStruct::OpenSessionExParams => (1, false),
            NvStruct::Config => match api {
                0xC2.. => (9, true),
                0xC0..=0xC1 => (8, true),
                _ => (7, true),
            },
            NvStruct::RcParams => (1, false),
            NvStruct::PresetConfig => (if api >= 0xC2 { 5 } else { 4 }, true),
            NvStruct::InitializeParams => match api {
                0xC2.. => (7, true),
                0xC1 => (6, true),
                _ => (5, true),
            },
            NvStruct::ReconfigureParams => (if api >= 0xC2 { 2 } else { 1 }, true),
            NvStruct::RegisterResource => match api {
                0xC2.. => (5, false),
                0xC0..=0xC1 => (4, false),
                _ => (3, false),
            },
            NvStruct::MapInputResource => (4, false),
            NvStruct::CreateBitstreamBuffer => (1, false),
            NvStruct::PicParams => match api {
                0xC2.. => (7, true),
                0xC0..=0xC1 => (6, true),
                _ => (4, true),
            },
            NvStruct::LockBitstream => match api {
                0xC2.. => (2, true),
                0xC1 => (1, true),
                0xC0 => (2, false),
                _ => (1, false),
            },
            NvStruct::CapsParam => (1, false),
        }
    }
}

/// Assemble the `NVENCAPI_STRUCT_VERSION` word for struct `s` at API version `(maj, min)`.
///
/// The 32-bit word packs, from `NvStruct::rev` and the API version:
///
/// 1. **API major** in bits 0-7, **API minor** in bits 24-27.
/// 2. **Struct revision** (`rev`) in bits 16-23.
/// 3. **Magic `0x7`** in bits 28-30.
/// 4. **The `1<<31` flag** in bit 31, when this struct sets it at this version.
///
/// For the pinned nvcodec-sys headers this reproduces the compile-time `NV_ENC_*_VER` constants
/// exactly, so a current driver is stamped byte-for-byte identically to its own SDK's constant; the
/// `version_tests` module asserts that identity.
fn nvenc_struct_ver(s: NvStruct, maj: u32, min: u32) -> u32 {
    let (rev, high_bit) = s.rev((maj << 4) | (min & 0xF));
    (maj & 0xFF) | ((min & 0xF) << 24) | (rev << 16) | (0x7 << 28) | ((high_bit as u32) << 31)
}

/// The process's effective NVENC API version `(major, minor)`: the negotiated value once
/// `nvenc_negotiate` has run, otherwise the pinned `NVENCAPI_VERSION` decomposed (major in the low
/// byte, minor at bit 24) as the pre-negotiation fallback.
#[inline]
fn nvenc_cur_ver() -> (u32, u32) {
    NVENC_NEG_VER
        .get()
        .copied()
        .unwrap_or((NVENCAPI_VERSION & 0xFF, (NVENCAPI_VERSION >> 24) & 0xFF))
}

/// The struct-version word for `s` tagged with the process's negotiated API version — the
/// value every NVENCAPI struct literal assigns to its `version` field.
#[inline]
fn sv(s: NvStruct) -> u32 {
    let (m, n) = nvenc_cur_ver();
    nvenc_struct_ver(s, m, n)
}

/// The negotiated session `apiVersion` word (`major | minor<<24`) passed to
/// `NvEncOpenEncodeSessionEx` — note the minor sits at bit 24 here, unlike the `(major<<4)|minor`
/// packing that `NvStruct::rev` matches on.
#[inline]
fn neg_api() -> u32 {
    let (m, n) = nvenc_cur_ver();
    m | (n << 24)
}

/// A struct as long as the negotiated API's layout of it, which can run eight bytes past the
/// pinned header's: `NV_ENC_INITIALIZE_PARAMS` below 12.2, and so the reconfigure params that
/// embed it, and `NV_ENC_LOCK_BITSTREAM` at 12.1. The tail is zeroed and pixelflux's, so the
/// driver reads and writes inside memory this owns.
#[repr(C)]
struct Negotiated<T> {
    value: T,
    tail: [u32; 2],
}

impl<T> Negotiated<T> {
    fn new(value: T) -> Self {
        Self {
            value,
            tail: [0; 2],
        }
    }
}

/// The reconfigure params re-initializing with `init` at API `(maj, min)`, resetting the encoder
/// and forcing an IDR as asked; below 12.2 the driver reads the two flags from the tail.
fn reconfigure_params(
    init: NV_ENC_INITIALIZE_PARAMS,
    (maj, min): (u32, u32),
    reset: bool,
    force_idr: bool,
) -> Negotiated<NV_ENC_RECONFIGURE_PARAMS> {
    let mut p = Negotiated::new(NV_ENC_RECONFIGURE_PARAMS {
        version: nvenc_struct_ver(NvStruct::ReconfigureParams, maj, min),
        reInitEncodeParams: init,
        ..Default::default()
    });
    if (maj << 4) | min < 0xC2 {
        p.tail[0] = reset as u32 | (force_idr as u32) << 1;
    } else {
        p.value.set_resetEncoder(reset as u32);
        p.value.set_forceIDR(force_idr as u32);
    }
    p
}

/// Resolve the process-wide NVENC API version once, by probing the driver newest-first and
/// remembering the highest version it accepts.
///
/// The bundled nv-codec-headers are NVENC 13.0 (`pinned`), so a current driver negotiates 13.0
/// natively while older drivers down-negotiate through 12.x / 11.x to the 10.0 floor (~R445). The
/// compiled struct *layouts* are the 13.0 ones, handed to older drivers with the tail their own
/// layouts run longer (`Negotiated`); only the version *words* change per negotiated version
/// (via the `NvStruct::rev` table), so each struct is stamped with the exact word the negotiated
/// SDK defined. Steps:
///
/// 1. **Cap the search** by the driver's max: query `get_max_version` when present, then optionally
///    lower it further from `PIXELFLUX_NVENC_MAX_API` (e.g. `"11.0"`) for testing / pinning. A cap
///    of 0 means unknown, and probing then relies on `create_instance` acceptance alone.
/// 2. **Probe candidates** newest-first (`pinned`, 12.1, 12.0, 11.1, 11.0, 10.0), skipping any
///    above the cap. Each probe stamps an `NV_ENCODE_API_FUNCTION_LIST` with that version's word and
///    calls `create_instance`.
/// 3. **Require the whole encode path**, not just a success code: the session opener plus
///    `nvEncInitializeEncoder`, `nvEncGetEncodePresetConfigEx`, `nvEncEncodePicture`, and
///    `nvEncLockBitstream` must all be non-null, because a driver can accept the function-list word
///    yet leave newer entry points null. The first fully-populated version wins.
/// 4. **Fall back** to `pinned` if nothing qualifies. Stored in `NVENC_NEG_VER`, set-once.
fn nvenc_negotiate(lib: &NvencLibrary) {
    NVENC_NEG_VER.get_or_init(|| {
        let pinned = (NVENCAPI_VERSION & 0xFF, (NVENCAPI_VERSION >> 24) & 0xFF);
        let mut drv_max: u32 = 0;
        if let Some(get_max) = lib.get_max_version {
            let mut m: u32 = 0;
            if unsafe { get_max(&mut m) } == NVENCSTATUS::NV_ENC_SUCCESS {
                drv_max = m;
            }
        }
        if let Ok(cap) = std::env::var("PIXELFLUX_NVENC_MAX_API") {
            let mut it = cap.split('.');
            if let (Some(a), Some(b)) = (it.next(), it.next())
                && let (Ok(cm), Ok(cn)) = (a.parse::<u32>(), b.parse::<u32>())
            {
                let capv = (cm << 4) | (cn & 0xF);
                if capv != 0 && (drv_max == 0 || capv < drv_max) {
                    drv_max = capv;
                }
            }
        }
        let candidates = [pinned, (12, 1), (12, 0), (11, 1), (11, 0), (10, 0)];
        for (maj, min) in candidates {
            let vv = (maj << 4) | min;
            if drv_max != 0 && vv > drv_max {
                continue;
            }
            let mut probe = NV_ENCODE_API_FUNCTION_LIST {
                version: nvenc_struct_ver(NvStruct::FunctionList, maj, min),
                ..Default::default()
            };
            let st = unsafe { (lib.create_instance)(&mut probe) };
            if st == NVENCSTATUS::NV_ENC_SUCCESS
                && probe.nvEncOpenEncodeSessionEx.is_some()
                && probe.nvEncInitializeEncoder.is_some()
                && probe.nvEncGetEncodePresetConfigEx.is_some()
                && probe.nvEncEncodePicture.is_some()
                && probe.nvEncLockBitstream.is_some()
            {
                crate::log::debug!("[pixelflux] NVENC API version negotiated: {}.{}", maj, min);
                return (maj, min);
            }
        }
        pinned
    });
}

/// Cached CUDA import of a dmabuf, keyed by fd so a recurring capture buffer is imported
/// once: the `EGLImageKHR`, the CUDA graphics resource it registers as, the mapped `CUeglFrame`,
/// and how that frame reaches NVENC. Torn down on drop / reconfigure, or evicted when the fd it is
/// keyed by no longer names the same buffer (see `DmaBufIdentity`).
struct CachedDmaBuf {
    identity: DmaBufIdentity,
    egl_image: EGLImageKHR,
    cuda_resource: CUgraphicsResource,
    egl_frame: CUeglFrame,
    input: DmaBufInput,
    /// The texture the chroma convert reads an array-typed import through, so its RGB is never
    /// copied into linear memory. Zero where the import is linear or carries no convert.
    tex: CUtexObject,
}

/// External input registrations an encoder keeps at once (`register_external_input`).
pub(crate) const EXTERNAL_INPUTS: usize = 3;

/// An input surface the session does not own: a device pointer produced by another component on
/// the same CUDA context — the NvFBC capture buffer — registered and mapped with NVENC in place
/// so the encoder reads the captured frame where it was composited.
///
/// The registration is kept for as long as the geometry holds, because a capture source hands
/// back the same buffer, or the same few in turn, every frame; it is released when the geometry
/// changes, and always before the component that owns the memory tears it down.
#[derive(Clone, Copy)]
struct ExternalInput {
    device_ptr: CUdeviceptr,
    pitch: usize,
    width: u32,
    height: u32,
    format: NV_ENC_BUFFER_FORMAT,
    registered: NV_ENC_REGISTERED_PTR,
    mapped: NV_ENC_INPUT_PTR,
}

/// How a cached dmabuf import feeds the encoder.
///
/// `Direct` is the zero-copy case: the mapped frame's first plane is itself registered and mapped
/// as an NVENC input — as a pitch-linear device pointer or as a CUDA array, per `DirectPlane` — so
/// encoding reads the capture buffer in place. `Copy` covers the rest: a plane `direct_plane`
/// rules out, one the driver declined to register, or the direct path switched off; the plane is
/// then copied into the session's packed input surface each frame.
#[derive(Clone, Copy)]
enum DmaBufInput {
    Direct {
        registered: NV_ENC_REGISTERED_PTR,
        mapped: NV_ENC_INPUT_PTR,
        format: NV_ENC_BUFFER_FORMAT,
    },
    Copy,
}

/// The NVENC packed input format whose byte order is a dmabuf's: the XR24 / AR24 family is
/// B,G,R,A in memory (NVENC's word-ordered `ARGB`), the XB24 / AB24 family R,G,B,A (`ABGR`).
/// `None` for any other fourcc — nothing NVENC reads as packed 8-bit RGB.
/// The fourcc a dmabuf is described with for CUDA's EGL import: the alpha-carrying twin of an
/// X-format, since the NVIDIA driver refuses XRGB/XBGR images there while the bytes lie identically
/// and NVENC ignores the alpha channel of ARGB/ABGR input.
fn egl_import_fourcc(code: Fourcc) -> Fourcc {
    match code {
        Fourcc::Xrgb8888 => Fourcc::Argb8888,
        Fourcc::Xbgr8888 => Fourcc::Abgr8888,
        other => other,
    }
}

fn fourcc_nvenc_format(code: Fourcc) -> Option<NV_ENC_BUFFER_FORMAT> {
    match code {
        Fourcc::Argb8888 | Fourcc::Xrgb8888 => {
            Some(NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB)
        }
        Fourcc::Abgr8888 | Fourcc::Xbgr8888 => {
            Some(NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ABGR)
        }
        _ => None,
    }
}

/// How the first plane of a mapped `CUeglFrame` registers with NVENC in place: the
/// `NV_ENC_REGISTER_RESOURCE` resource type and the `pitch` word that type expects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirectPlane {
    /// Pitch-linear device memory, registered as a CUDA device pointer at this row pitch.
    Pitch(u32),
    /// A two-dimensional CUDA array of four 8-bit channels, registered as a CUDA array; the
    /// value is the array's row width in bytes (`Width × NumChannels`), which is what NVENC
    /// takes as the pitch of an array resource.
    Array(u32),
}

/// Whether NVENC can read a mapped `CUeglFrame` in place, and how.
///
/// Either frame kind has to be usable as the session's `width × height` input: a first plane
/// present and non-null, and a geometry of at least the session's. A pitch-linear plane also needs
/// a row pitch covering `width * 4` bytes at the 4-byte alignment `NV_ENC_REGISTER_RESOURCE`
/// requires; a CUDA-array plane has to be four 8-bit channels, the layout NVENC's packed formats
/// describe. `None` sends the frame down the per-frame copy into the session's own input surface.
fn direct_plane(frame: &CUeglFrame, width: u32, height: u32) -> Option<DirectPlane> {
    if frame.plane_count < 1 || width == 0 || height == 0 {
        return None;
    }
    if frame.width < width || frame.height < height {
        return None;
    }
    match frame.frame_type {
        CU_EGL_FRAME_TYPE_PITCH => {
            let plane = unsafe { frame.frame.p_pitch[0] };
            let pitch_ok = frame.pitch >= width.saturating_mul(4) && frame.pitch.is_multiple_of(4);
            (!plane.is_null() && pitch_ok).then_some(DirectPlane::Pitch(frame.pitch))
        }
        CU_EGL_FRAME_TYPE_ARRAY => {
            let array = unsafe { frame.frame.p_array[0] };
            let packed_8bit = frame.cu_format == CU_AD_FORMAT_U8 && frame.num_channels == 4;
            (!array.is_null() && packed_8bit).then_some(DirectPlane::Array(frame.width * 4))
        }
        _ => None,
    }
}

/// The chroma format and dimensional feasibility a session settles on given the driver's
/// reported capabilities, so init degrades cleanly instead of failing opaquely.
///
/// - `fullcolor` is the chroma actually used: 4:4:4 only when it was requested and the GPU carries
///   it, otherwise 4:2:0.
/// - `downgraded_color` records that a 4:4:4 request was met with 4:2:0, so the caller says so once.
/// - `too_large` carries the driver's `(max_w, max_h)` when the requested geometry exceeds it; the
///   caller then declines NVENC and falls back to software rather than failing to initialize.
///
/// A capability that could not be queried is `None` and does not gate: 4:4:4 stays as requested and
/// the dimension test is skipped, so an unavailable answer never forces a false downgrade or refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CapsDecision {
    fullcolor: bool,
    downgraded_color: bool,
    too_large: Option<(i32, i32)>,
}

/// Resolve the requested chroma and geometry against the driver caps (`None` = unknown, ungated).
fn decide_caps(
    req_fullcolor: bool,
    req_w: i32,
    req_h: i32,
    cap_yuv444: Option<i32>,
    cap_width_max: Option<i32>,
    cap_height_max: Option<i32>,
) -> CapsDecision {
    let downgraded_color = req_fullcolor && cap_yuv444 == Some(0);
    let fullcolor = req_fullcolor && !downgraded_color;
    let exceeds = |req: i32, cap: Option<i32>| cap.is_some_and(|m| m > 0 && req > m);
    let too_large = if exceeds(req_w, cap_width_max) || exceeds(req_h, cap_height_max) {
        Some((cap_width_max.unwrap_or(0), cap_height_max.unwrap_or(0)))
    } else {
        None
    };
    CapsDecision {
        fullcolor,
        downgraded_color,
        too_large,
    }
}

/// The geometry every session is initialized to hold, so `reconfigure_resolution` can grow into
/// it in place. It spans both UHD and DCI 4K and is the largest picture the 5.x levels of all
/// three codecs admit, which is what lets AV1 -- pinned to this level -- stay inside a level a
/// decoder accepts; a taller resize rebuilds the session instead.
const HEADROOM_WIDTH: u32 = 4096;
const HEADROOM_HEIGHT: u32 = 2160;

/// The frame rate the level is read at when the session's is lower, so a live change between
/// the common rates keeps the level, as the geometry headroom keeps it across a resize.
const HEADROOM_FPS: u32 = 60;

/// The frames a reference window holds for a decoded picture buffer of `dpb`: an H.265 session
/// keeping anchors still reaches, on Pascal's NVENC, the frame the next frame's reference picture
/// set lets go, one past the buffer it declares. Turing's, Ampere's and Ada's do not, and
/// predict from the anchor before that frame where the window names it: a dependency the window
/// states no older than the real one, so a client holding the frame named holds the one used.
fn window_frames(codec: Codec, dpb: u32, anchored: bool) -> u32 {
    dpb + u32::from(codec == Codec::H265 && anchored)
}

/// Persistent anchors only where the negotiated API exposes them, the device can invalidate
/// references and the DPB leaves two recent pictures beside them. AV1 gained the LTR fields in
/// SDK 13; drivers negotiated down to an older API keep the unanchored path. AV1 keeps one, a
/// frame every forty-eight, unless the consumer `acknowledged` what its clients hold and the
/// device has room for two: then the newest every client holds is pinned beside the one the
/// schedule marks. H.264 keeps one whatever the consumer says: OpenH264's decoder, Firefox's for
/// WebRTC H.264 where it has no FFmpeg that decodes it, finds a long-term picture of a CABAC
/// stream by a number no marking sets, so of two it reaches only the first, and decodes a frame
/// naming the second against another picture without an error; and a key frame marked
/// long-term leaves a second index past `MaxLongTermFrameIdx` besides.
fn anchor_count(
    codec: Codec,
    api_major: u32,
    invalidation: bool,
    ltr: Option<i32>,
    dpb: u32,
    acknowledged: bool,
) -> usize {
    let fits =
        |n: usize| invalidation && ltr.is_some_and(|l| l >= n as i32) && dpb as usize >= n + 2;
    let count = match codec {
        Codec::Av1 if api_major < 13 => 0,
        Codec::H265 => ANCHORS,
        Codec::Av1 if acknowledged && fits(ANCHORS) => ANCHORS,
        _ => 1,
    };
    if fits(count) { count } else { 0 }
}

/// The in-place resize headroom for one axis: the requested size lifted to `floor` but never past
/// the driver's reported maximum, so initializing with headroom cannot itself exceed what the GPU
/// supports.
fn nvenc_headroom(size: u32, floor: u32, cap: Option<i32>) -> u32 {
    let want = size.max(floor);
    match cap {
        Some(m) if m > 0 => want.min(m as u32),
        _ => want,
    }
}

/// The share of an AV1 level's Annex A MaxBitrate the strictest driver lets a session declare
/// at it, as a fraction: two thirds on 595.71.05, where 595.91.07 and 615.71.09 take the whole.
const NVENC_AV1_RATE: (u64, u64) = (2, 3);

/// Whether the process's driver holds an AV1 level to `NVENC_AV1_RATE` of its Annex A rate: set
/// where it refused a session or a rate change at the Annex A level and took the weighted one,
/// so later sessions declare the weighted level without the refusal.
static NVENC_AV1_WEIGHTED: AtomicBool = AtomicBool::new(false);

/// Whether the process's driver refused a CBR session's quarter-resolution first pass, as
/// Kepler's does, taking only a full-resolution one: later sessions code CBR in a single pass
/// without the refusal. On a Tesla K80 the full-resolution pass encodes a 1080p H.264 frame in
/// 12.7 ms where the single pass takes 7.3 ms and overshoots its target by 4%.
static NVENC_SINGLE_PASS: AtomicBool = AtomicBool::new(false);

/// The rate a `codec` level has to admit for a peak of `bps`: AV1's weighed by `NVENC_AV1_RATE`
/// where the driver holds a level to that share.
fn level_rate(codec: Codec, bps: u64, weighted: bool) -> u64 {
    if codec == Codec::Av1 && weighted {
        (bps * NVENC_AV1_RATE.1).div_ceil(NVENC_AV1_RATE.0)
    } else {
        bps
    }
}

/// The highest CBR target an NVENC session of `codec` opens at, its top level's bitrate ceiling:
/// H.264 6.2's 1 Gbit/s, HEVC 6.2's 800 Mbit/s at the High tier production sessions declare,
/// and `NVENC_AV1_RATE` of AV1 6.3's Main-tier 160 Mbit/s. No level admits a rate past it, so
/// the driver would refuse the session, or the change, as an invalid level.
fn nvenc_rate_ceiling(codec: Codec) -> u32 {
    match codec {
        Codec::Av1 => (160_000_000 * NVENC_AV1_RATE.0 / NVENC_AV1_RATE.1) as u32,
        Codec::H265 => 800_000_000,
        _ => 1_000_000_000,
    }
}

/// The split-frame mode a `codec` session asks for at `width` x `height` on a device of `engines`
/// NVENC engines under the negotiated `api`, as `gpu_bench_tuning_on_content` measured it. AV1
/// splits across the engines whatever the picture: a 1080p frame encodes in 1.36 ms rather than
/// 2.07 on an RTX 4090, a 1440p one in 2.27 rather than 3.58, and both code as well or better,
/// where the driver's own choice splits only from 4K. HEVC splits from a 4K picture, which the
/// driver's choice splits on Ada but not on Pascal: a GTX 1080 encodes the 2160p frame in 10.3 ms
/// rather than 16.1, more than 5% of the 81 ms such a session measures glass to glass, for 2.5 dB
/// of PSNR on scrolling text. Below 4K a split saves HEVC about a millisecond for up to 2.3 dB,
/// so the driver decides there, and H.264 never splits. The forced mode cuts two strips on three
/// engines as on two, so AV1 asks for three there: an L40S encodes a 1080p frame in 1.45 ms
/// rather than 1.62 and a 2160p one in 4.2 rather than 5.3, coding better in five of six rows
/// (up to 1.5 dB on text) and 0.5 dB worse in one, where HEVC's third strip saves nothing at 4K
/// and costs 0.1 to 0.2 dB. Before API 12.1 the field's bits are another flag's, so an older
/// session leaves them clear.
fn split_mode(
    codec: Codec,
    width: u32,
    height: u32,
    engines: Option<i32>,
    api: (u32, u32),
) -> NV_ENC_SPLIT_ENCODE_MODE {
    let forced = api >= (12, 1)
        && engines.is_some_and(|n| n > 1)
        && match codec {
            Codec::Av1 => true,
            Codec::H265 => width as u64 * height as u64 >= 3840 * 2160,
            _ => false,
        };
    match (forced, codec, engines) {
        (true, Codec::Av1, Some(3)) => NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_THREE_FORCED_MODE,
        (true, ..) => NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_AUTO_FORCED_MODE,
        _ => NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_AUTO_MODE,
    }
}

/// The CBR target of a session at `settings`, in bits per second, held to the codec's
/// `nvenc_rate_ceiling`.
fn cbr_bps(settings: &CaptureSettings) -> u32 {
    (settings.video_bitrate_kbps.max(0) as u32)
        .saturating_mul(1000)
        .min(nvenc_rate_ceiling(settings.codec))
}

/// Say so where `cbr_bps` held the target `settings` asks for to the codec's ceiling.
fn log_held_rate(settings: &CaptureSettings, bps: u32) {
    let asked = settings.video_bitrate_kbps.max(0) as u64 * 1000;
    if (bps as u64) < asked {
        eprintln!(
            "[NVENC] {} opens at {} kbps at most, its top level's ceiling: {} kbps is held there",
            settings.codec.display(),
            bps / 1000,
            asked / 1000
        );
    }
}

/// The VBV of a CBR session at `bps`, from the session's frame rate, key-frame interval, and
/// explicit multiplier.
fn cbr_vbv(settings: &CaptureSettings, bps: u32) -> u32 {
    crate::encoders::vbv_bits(
        bps,
        settings.target_fps,
        settings.keyframe_interval_s,
        settings.video_vbv_multiplier,
    )
}

/// Write a CBR target into `rc`: the average and peak rate, the VBV, and an initial delay of the
/// whole buffer. That delay is the driver's default (`gpu_bench_cbr_policy` finds the two
/// identical), stated so the HRD's starting point is explicit and a rate change restates it
/// with the buffer it belongs to.
fn set_cbr_rate(rc: &mut NV_ENC_RC_PARAMS, bps: u32, vbv: u32) {
    rc.averageBitRate = bps;
    rc.maxBitRate = bps;
    rc.vbvBufferSize = vbv;
    rc.vbvInitialDelay = vbv;
}

/// The peak a capped constant quantizer runs under (`RateSettings::capped`), as NVENC's
/// constant rate takes it: the target held to the codec's ceiling, and its buffer.
///
/// NVENC's constant quantizer takes no cap, and its quality-targeting VBR codes a picture of its
/// own rather than the constant quantizer's, so the session switches (`PeakBucket`): it codes at
/// its constant quantizer until its output overflows the peak, then through the constant rate at
/// the peak with the quantizer as its `minQP`, and back. The rate-control mode is one of the
/// parameters `nvEncReconfigureEncoder` takes without a reset (`hold_rate`), so neither switch
/// costs a key frame. A peak past the rate the level the session declared carries leaves it at
/// its constant quantizer, since a raised level is a key frame.
fn peak_of(settings: &CaptureSettings) -> Option<PeakBucket> {
    RateSettings::new(settings).capped.then(|| {
        let bps = cbr_bps(settings);
        PeakBucket::new(bps, cbr_vbv(settings, bps), settings.target_fps)
    })
}

/// Quality index steps finer than the paint-over one a refined block is coded at
/// (`FrameEncoder::refines`): what brings it to the quality NVENC's own cleanup of a still screen
/// reaches. 1080p text beside a video, H.264 on an RTX 3060, against that cleanup's 45.2 dB at
/// 8 Mbit/s, 47.6 at 16, and 47.0 at a constant quality: 44.4, 45.5, and 46.8 at the paint-over
/// quantizer, 45.9, 47.0, and 48.1 three steps finer, 48.2, 48.7, and 50.6 six steps finer,
/// spending on the text what the video would have had.
pub const REFINE_FINER: u32 = 3;

/// The share of its cap a held band taken back for coming out past it is coded again to, at the
/// bytes a block of it came to (`NvencEncoder::cap`): the next rows of a page can be denser.
const REFIT_SHARE: f64 = 0.8;

/// Recent frames whose held bands the session keeps (`forget_bands`): past how far back an
/// invalidation of its own before an anchor reaches (`reference::HOLD_MAX`, 384).
const BAND_LOG_FRAMES: usize = 512;

/// The band a frame codes once the frames from timestamp `pts` on are left out of every
/// prediction, their bands taken out of `log`: the one it holds (`held`) reaching back over theirs;
/// and whether theirs are lost, a frame that holds no quantizer (`holds`) taking none of them back.
/// A frame held whole codes them anyway.
fn reach_back(
    log: &mut VecDeque<(u64, f64, f64)>,
    pts: u64,
    held: Option<(f64, f64)>,
    holds: bool,
) -> (Option<(f64, f64)>, bool) {
    let from = log
        .iter()
        .filter(|b| b.0 >= pts)
        .map(|b| b.1)
        .reduce(f64::min);
    log.retain(|b| b.0 < pts);
    match (from, held) {
        (Some(from), Some((a, b))) => (Some((a.min(from), b)), false),
        (Some(_), None) => (None, !holds),
        (None, held) => (held, false),
    }
}

/// Fill `map`, the QP delta map of a `width` x `height` picture of `codec` (one entry a 16x16
/// macroblock for H.264, a 32x32 coding tree block for HEVC, a 64x64 superblock for AV1, in
/// raster order), with `delta` over the blocks the share of the picture `from..to` covers,
/// rounded outward, and 0 elsewhere; returns the band's first block.
fn fill_band_map(
    map: &mut Vec<i8>,
    codec: Codec,
    width: u32,
    height: u32,
    (from, to): (f64, f64),
    delta: i32,
) -> usize {
    let block = match codec {
        Codec::H264 => 16,
        Codec::H265 => 32,
        _ => 64,
    };
    let blocks = (width.div_ceil(block) * height.div_ceil(block)) as usize;
    let first = ((from.clamp(0.0, 1.0) * blocks as f64).floor() as usize).min(blocks);
    let last = ((to.clamp(0.0, 1.0) * blocks as f64).ceil() as usize).clamp(first, blocks);
    map.clear();
    map.resize(blocks, 0);
    map[first..last].fill(delta.clamp(-(codec.quantizer_max().min(128) as i32), 0) as i8);
    first
}

/// Hand `pic` the QP delta map `map` at API `api`. Below 12.0 the codec union runs eight bytes
/// shorter and the driver reads the map's pointer and size where the pinned layout keeps
/// `reserved5`.
fn point_at_qp_map(pic: &mut NV_ENC_PIC_PARAMS, map: &mut [i8], api: (u32, u32)) {
    if api < (12, 0) {
        pic.reserved5 = [
            map.as_mut_ptr().cast(),
            ptr::without_provenance_mut(map.len()),
        ];
    } else {
        pic.qpDeltaMap = map.as_mut_ptr();
        pic.qpDeltaMapSize = map.len() as u32;
    }
}

/// The driver's message for the last failure on `session`, or a stand-in when it has none: NVENC
/// clears the string once a session is torn down, so it is only meaningful read straight after
/// the call that failed.
unsafe fn last_error(funcs: &NV_ENCODE_API_FUNCTION_LIST, session: *mut c_void) -> String {
    funcs
        .nvEncGetLastErrorString
        .map(|f| f(session))
        .filter(|p| !p.is_null())
        .map(|p| CStr::from_ptr(p).to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "no error string".to_string())
}

/// Query one NVENC capability of `codec` on an open session, returning the driver's integer
/// answer or `None` when the entry point is absent or the query fails — `decide_caps` reads
/// `None` as "do not gate", so a query failure never becomes a false refusal.
unsafe fn query_cap(
    funcs: &NV_ENCODE_API_FUNCTION_LIST,
    session: *mut c_void,
    codec: GUID,
    cap: NV_ENC_CAPS,
) -> Option<i32> {
    let get = funcs.nvEncGetEncodeCaps?;
    let mut param = NV_ENC_CAPS_PARAM {
        version: sv(NvStruct::CapsParam),
        capsToQuery: cap,
        reserved: [0u32; 62],
    };
    let mut val: i32 = 0;
    if get(session, codec, &mut param, &mut val) == NVENCSTATUS::NV_ENC_SUCCESS {
        Some(val)
    } else {
        None
    }
}

/// GUID selecting the H.264 **High** profile (4:2:0) for `NV_ENC_CONFIG::profileGUID`.
const NV_ENC_H264_PROFILE_HIGH_GUID: GUID = GUID {
    Data1: 0x205b553d,
    Data2: 0x5f01,
    Data3: 0x4d9e,
    Data4: [0x91, 0x84, 0xda, 0x32, 0x77, 0x5b, 0x55, 0x9b],
};

/// GUID selecting the H.264 **High 4:4:4 Predictive** profile for full-color encoding.
const NV_ENC_H264_PROFILE_HIGH_444_GUID: GUID = GUID {
    Data1: 0x7ac663cb,
    Data2: 0xa598,
    Data3: 0x4960,
    Data4: [0xb8, 0x44, 0x33, 0x9b, 0x26, 0x1a, 0x7d, 0x5c],
};

/// Whether two GUIDs are the same.
fn guid_eq(a: &GUID, b: &GUID) -> bool {
    a.Data1 == b.Data1 && a.Data2 == b.Data2 && a.Data3 == b.Data3 && a.Data4 == b.Data4
}

/// The NVENC codec GUID of a codec the encoder serves, or `None` for one NVENC has no engine
/// for.
fn codec_guid(codec: Codec) -> Option<GUID> {
    match codec {
        Codec::H264 => Some(NV_ENC_CODEC_H264_GUID),
        Codec::H265 => Some(NV_ENC_CODEC_HEVC_GUID),
        Codec::Av1 => Some(NV_ENC_CODEC_AV1_GUID),
        Codec::Jpeg | Codec::Vp8 | Codec::Vp9 => None,
    }
}

/// The video codecs the NVENC of the GPU behind `encode_node_index` has an engine for, read
/// from a bare session's GUID list the way a real session reads it (`device_encodes`): the
/// driver is negotiated, the device bound by the render node's PCI bus id (the first CUDA
/// device where the node names none), and the session opened on its primary context with no
/// input buffers, EGL, or encoder initialization. An error names the step that failed: no
/// driver, no device, or a session that would not open, each of which a real session would
/// fail on too. The CUDA and NVENC libraries stay loaded like a session's, since the driver
/// does not promise to survive `libcuda` being unloaded after `cuInit`.
pub(crate) fn probe_codecs(encode_node_index: i32) -> Result<Vec<(Codec, super::Formats)>, String> {
    let cuda = std::mem::ManuallyDrop::new(NvencEncoder::load_cuda()?);
    let nvenc_lib = std::mem::ManuallyDrop::new(NvencEncoder::load_nvenc()?);
    nvenc_negotiate(&nvenc_lib);
    crate::nvgpufilter::install();
    unsafe {
        let res = (cuda.cuInit)(0);
        if res != CUresult::CUDA_SUCCESS {
            return Err(format!(
                "Init CUDA failed: {}",
                NvencEncoder::get_error_string(&cuda, res)
            ));
        }
        let mut cu_device: CUdevice = 0;
        let bound = NvencEncoder::get_pci_bus_id(encode_node_index.max(0))
            .and_then(|id| CString::new(id).ok())
            .is_some_and(|id| {
                (cuda.cuDeviceGetByPCIBusId)(&mut cu_device, id.as_ptr()) == CUresult::CUDA_SUCCESS
            });
        if !bound && (cuda.cuDeviceGet)(&mut cu_device, 0) != CUresult::CUDA_SUCCESS {
            return Err("Failed to get default CUDA device".into());
        }
        let mut cu_context: CUcontext = ptr::null_mut();
        if (cuda.cuDevicePrimaryCtxRetain)(&mut cu_context, cu_device) != CUresult::CUDA_SUCCESS {
            return Err("Failed to retain the device's primary CUDA context".into());
        }
        if (cuda.cuCtxPushCurrent_v2)(cu_context) != CUresult::CUDA_SUCCESS {
            (cuda.cuDevicePrimaryCtxRelease_v2)(cu_device);
            return Err("Failed to make the primary CUDA context current".into());
        }
        let result = probe_session_codecs(&nvenc_lib, cu_context);
        (cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
        (cuda.cuDevicePrimaryCtxRelease_v2)(cu_device);
        result
    }
}

/// The probe's answer where the device refuses a session for want of one to spare (the
/// session cap of consumer boards, or its memory): nothing lasting, unlike its other refusals.
pub(crate) const SESSIONS_TAKEN: &str = "the device has no NVENC session to spare";

/// Why the driver refused to open a session: `SESSIONS_TAKEN` for the statuses it answers when
/// none is left, `NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY` at the cap of a consumer board and
/// `NV_ENC_ERR_OUT_OF_MEMORY` when its memory runs out.
fn session_refusal(status: NVENCSTATUS) -> String {
    match status {
        NVENCSTATUS::NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY | NVENCSTATUS::NV_ENC_ERR_OUT_OF_MEMORY => {
            SESSIONS_TAKEN.into()
        }
        _ => "Failed to open NVENC session".into(),
    }
}

/// Whether a session of `codec` codes 10 bits: HEVC and AV1 where the device reports it, at
/// whichever API the session negotiated (`set_bit_depth`). The session converts its 8-bit RGB to
/// 10-bit samples itself (`ConvertLayout`). H.264 stays 8-bit, which only the newest engines code
/// at 10.
unsafe fn codes_ten_bit(
    function_list: &NV_ENCODE_API_FUNCTION_LIST,
    session: *mut c_void,
    codec: Codec,
    guid: GUID,
) -> bool {
    matches!(codec, Codec::H265 | Codec::Av1)
        && query_cap(
            function_list,
            session,
            guid,
            NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_10BIT_ENCODE,
        ) == Some(1)
}

/// Stamp an HEVC or AV1 `config` with `bit_depth` as API `api` reads it. From 12.2 the session
/// names its input and output depths; before it HEVC carries the output's minus eight in the
/// bitfield the pinned layout keeps as `reserved3`, its input following the buffer format, and
/// AV1 (12.0 and 12.1) the input's and the output's in the bits 13.0 gave its LTR, SVC and HDR
/// flags. The offsets count from the codec config's first bitfield, at byte 16.
fn set_bit_depth(config: &mut NV_ENC_CONFIG, codec: Codec, bit_depth: u32, api: (u32, u32)) {
    let depth = if bit_depth > 8 {
        NV_ENC_BIT_DEPTH::NV_ENC_BIT_DEPTH_10
    } else {
        NV_ENC_BIT_DEPTH::NV_ENC_BIT_DEPTH_8
    };
    let minus8 = u64::from(bit_depth.saturating_sub(8));
    unsafe {
        match codec {
            Codec::H265 if api >= (12, 2) => {
                let c = &mut config.encodeCodecConfig.hevcConfig;
                c.inputBitDepth = depth;
                c.outputBitDepth = depth;
            }
            Codec::H265 => config
                .encodeCodecConfig
                .hevcConfig
                ._bitfield_1
                .set(11, 3, minus8),
            Codec::Av1 if api >= (12, 2) => {
                let c = &mut config.encodeCodecConfig.av1Config;
                c.inputBitDepth = depth;
                c.outputBitDepth = depth;
            }
            Codec::Av1 => {
                let bits = &mut config.encodeCodecConfig.av1Config._bitfield_1;
                bits.set(12, 3, minus8);
                bits.set(15, 3, minus8);
            }
            _ => {}
        }
    }
}

/// Open a bare NVENC session on a current CUDA context, list the codecs its device encodes
/// and the formats past 8-bit 4:2:0 it encodes each in, and close it.
unsafe fn probe_session_codecs(
    nvenc_lib: &NvencLibrary,
    cu_context: CUcontext,
) -> Result<Vec<(Codec, super::Formats)>, String> {
    let mut function_list = NV_ENCODE_API_FUNCTION_LIST {
        version: sv(NvStruct::FunctionList),
        ..Default::default()
    };
    if (nvenc_lib.create_instance)(&mut function_list) != NVENCSTATUS::NV_ENC_SUCCESS {
        return Err("NvEncodeAPICreateInstance failed".into());
    }
    let (Some(open_fn), Some(destroy_fn)) = (
        function_list.nvEncOpenEncodeSessionEx,
        function_list.nvEncDestroyEncoder,
    ) else {
        return Err("the driver's NVENC function list has no session entry points".into());
    };
    let mut session_params = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
        version: sv(NvStruct::OpenSessionExParams),
        deviceType: NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_CUDA,
        device: cu_context as *mut c_void,
        apiVersion: neg_api(),
        ..Default::default()
    };
    let mut session: *mut c_void = ptr::null_mut();
    match open_fn(&mut session_params, &mut session) {
        NVENCSTATUS::NV_ENC_SUCCESS => {}
        status => return Err(session_refusal(status)),
    }
    let codecs = Codec::VIDEO
        .into_iter()
        .filter_map(|codec| {
            let guid = codec_guid(codec)
                .filter(|guid| NvencEncoder::device_encodes(&function_list, session, guid))?;
            let fullcolor = codec.fullcolor()
                && query_cap(
                    &function_list,
                    session,
                    guid,
                    NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_YUV444_ENCODE,
                ) == Some(1);
            let ten_bit = codes_ten_bit(&function_list, session, codec, guid);
            Some((
                codec,
                super::Formats {
                    fullcolor,
                    ten_bit: [ten_bit, ten_bit && fullcolor],
                },
            ))
        })
        .collect();
    destroy_fn(session);
    Ok(codecs)
}

/// `sliceMode = 3`: `sliceModeData` is the number of slices in the picture, which the driver
/// divides evenly; the other modes count macroblocks, bytes, or rows and drift with geometry.
const SLICE_MODE_COUNT: u32 = 3;

/// Slices per H.264 frame, the count the VA-API (`SLICES`; one for H.264 on AMD's VCE) and
/// OpenH264 (`SM_FIXEDSLCNUM_SLICE`) sessions emit too: a client decoding in software threads a
/// frame across its slices, and more than four upsets some Chromium decoders. What the slices cost
/// at a fixed quantizer is measured by `gpu_bench_slices`. An HEVC frame is one slice, since no
/// decoder a client has threads HEVC's plain slices (FFmpeg threads its wavefronts and tiles), and
/// its band frames take theirs where a band starts (`set_band_layout`). AV1 partitions by tiles
/// instead, asked for as 1x1 in `configure_codec`.
const SLICES_PER_FRAME: u32 = 4;

/// The most slices an HEVC band frame takes to start one at its band (`set_band_layout`). NVENC
/// sizes a frame's slices evenly, so a band starting within the first eighth of the picture would
/// take more, each a header a low rate's frame cannot spare: at 60 fps and 0.5 Mbit/s, 40 to 60 of
/// them took most of a band frame's bytes and left its band coarse.
const BAND_SLICES_MAX: usize = 8;

/// The frames an AV1 session keeps, as many as a frame predicts from, LAST, LAST2, LAST3, and
/// GOLDEN (`numFwdRefs` tops out at four). The device keeps eight as well, predicting past a loss
/// from further back, but takes 112 MiB more a session at the resize headroom. With SDK 13
/// and LTR support, the anchors among them let an invalidation reach further back.
const AV1_REFERENCES: u32 = 4;

/// Output bitstream buffers per session: one, because `submit_frame` locks, copies, and unlocks
/// each frame's bitstream before it returns, so no second buffer is ever outstanding (the lock
/// blocks; a `doNotWait` lock on Linux answers an unfinished encode with an empty bitstream
/// rather than `NV_ENC_ERR_LOCK_BUSY`). The ring stays, so a pipelined depth is one constant
/// away.
const BITSTREAM_BUFFERS: usize = 1;

/// The encoder-side quality knobs of a session: the preset, the rate-control passes of a CBR
/// session, and adaptive quantization. Production sessions take the default, which the tuning
/// bench (`gpu_bench_tuning`) measures against the alternatives: P3 encodes a 1080p H.264
/// frame in 3.7 ms where P4 takes 5.2 ms on a V100, for 0.001 of SSIM at the same bitrate,
/// and the presets above P4 buy nothing; a single pass saves half a millisecond but overshoots
/// a CBR target by five to twelve percent; adaptive quantization moves neither time nor
/// quality measurably. The quarter-resolution first pass needs the 1.5-frame VBV `vbv_bits`
/// gives it: on a one-frame buffer its miss on a scene cut runs an 8 Mbit/s H.264 session at
/// 13.7 Mbit/s (`gpu_bench_cbr_rate_control`). P3 holds on real content as well
/// (`gpu_bench_tuning_on_content`, scrolling text and a panned texture at 1080p to 2160p): on a
/// GTX 1080 and an RTX 4090 P1 encodes 0.5 to 2.4 ms sooner and P2 less, at most about 5% of the
/// glass to glass latency a session of that size measures (25, 41, and 81 ms over WebSockets),
/// and both code the text worse, by up to 0.8 dB in H.264 and 2.4 dB in AV1.
#[derive(Clone, Copy, Debug)]
pub(crate) struct NvencTuning {
    pub preset: GUID,
    pub multipass: NV_ENC_MULTI_PASS,
    pub spatial_aq: bool,
    /// Temporal adaptive quantization, refused at open where the driver reports no support for
    /// it: a Volta HEVC session given it faults inside its first encode instead of failing to
    /// open.
    pub temporal_aq: bool,
    /// Whether an HEVC session declares the tier `h265_tier` names for its level; off opens the
    /// same session at Main tier, the ceiling `gpu_hevc_high_tier_opens_above_the_main_tier_ceiling`
    /// measures.
    pub hevc_high_tier: bool,
    /// Slices per H.264 and HEVC frame outside a band frame's: `None` in production, which is
    /// `SLICES_PER_FRAME` for H.264 and one for HEVC; a count to measure what slices cost
    /// (`gpu_bench_slices`).
    pub slices: Option<u32>,
    /// Leave the chroma convert out, so the session encodes through NVENC's own conversion —
    /// the path a driver refusing the kernel takes, which
    /// `gpu_hardware_conversion_matches_the_declared_matrix` measures.
    #[cfg(test)]
    pub hardware_csc: bool,
    /// The L0 references of an H.264 or HEVC frame, AV1's forward references: the preset's
    /// choice in production, where one reference saves no time, set to measure.
    #[cfg(test)]
    pub ref_l0: Option<NV_ENC_NUM_REF_FRAMES>,
    /// A split-frame mode in place of `split_mode`'s, to measure.
    #[cfg(test)]
    pub split: Option<NV_ENC_SPLIT_ENCODE_MODE>,
}

impl Default for NvencTuning {
    fn default() -> Self {
        Self {
            preset: NV_ENC_PRESET_P3_GUID,
            multipass: NV_ENC_MULTI_PASS::NV_ENC_TWO_PASS_QUARTER_RESOLUTION,
            spatial_aq: false,
            temporal_aq: false,
            hevc_high_tier: true,
            slices: None,
            #[cfg(test)]
            hardware_csc: false,
            #[cfg(test)]
            ref_l0: None,
            #[cfg(test)]
            split: None,
        }
    }
}

/// The profile of a session: the 4:2:0 profile of the codec at its depth, or its 4:4:4 one
/// where the codec has it. HEVC's range extensions carry 4:4:4 at either depth, and AV1's main
/// profile both depths.
fn profile_guid(codec: Codec, fullcolor: bool, bit_depth: u32) -> GUID {
    match (codec, fullcolor) {
        (Codec::H264, true) => NV_ENC_H264_PROFILE_HIGH_444_GUID,
        (Codec::H264, false) => NV_ENC_H264_PROFILE_HIGH_GUID,
        (Codec::H265, true) => NV_ENC_HEVC_PROFILE_FREXT_GUID,
        (Codec::H265, false) if bit_depth > 8 => NV_ENC_HEVC_PROFILE_MAIN10_GUID,
        (Codec::H265, false) => NV_ENC_HEVC_PROFILE_MAIN_GUID,
        _ => NV_ENC_AV1_PROFILE_MAIN_GUID,
    }
}

/// The 4:2:0 convert that replaces NVENC's own, because the hardware's fixed-function
/// conversion weights the two columns of a block 3:1 instead of averaging them, leaving half the
/// color of a subpixel-antialiased glyph edge in the chroma plane where the software and VA-API
/// converts leave none. It follows the matrix the session declares
/// (`gpu_hardware_conversion_matches_the_declared_matrix`), so only the siting is at stake, and
/// 4:4:4 — which subsamples nothing — keeps it.
///
/// The kernels ship as PTX the driver JIT-compiles (`cuModuleLoadData`), so the only library
/// involved is the `libcuda` NVENC already needs — nothing to install, and no runtime compiler.
/// They read the session's packed ARGB surface, or a texture over an array-typed dmabuf import,
/// and write the NV12 surface NVENC then encodes, averaging each block's RGB before the matrix
/// as the host convert does.
/// The surface a `ChromaConvert` writes for NVENC: NV12 for an 8-bit 4:2:0 session, and for a
/// 10-bit one P010 or planar 4:4:4, whose samples the kernel computes from the 8-bit RGB at
/// 10 bits. NVENC upconverts an 8-bit surface itself, but from samples already rounded to
/// eight: a 4:4:4 HEVC session coded that way measured 41.6 dB against its 8-bit 45.5 at one
/// quantizer on an RTX 3060, its luma half a level low.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConvertLayout {
    Nv12,
    P010,
    Yuv444P10,
}

impl ConvertLayout {
    /// The layout a session of this chroma and depth converts into; None for 8-bit 4:4:4,
    /// which subsamples nothing and keeps NVENC's own conversion.
    fn of(fullcolor: bool, bit_depth: u32) -> Option<Self> {
        match (fullcolor, bit_depth > 8) {
            (false, false) => Some(Self::Nv12),
            (false, true) => Some(Self::P010),
            (true, true) => Some(Self::Yuv444P10),
            (true, false) => None,
        }
    }

    /// The kernels reading a linear surface and a texture.
    fn kernels(self) -> (&'static CStr, &'static CStr) {
        match self {
            Self::Nv12 => (c"argb_to_nv12", c"argb_tex_to_nv12"),
            Self::P010 => (c"argb_to_p010", c"argb_tex_to_p010"),
            Self::Yuv444P10 => (c"argb_to_yuv444p10", c"argb_tex_to_yuv444p10"),
        }
    }

    /// The bytes a row and the rows a `width`x`height` picture take: a luma plane and half as
    /// many rows of interleaved chroma, or three whole planes.
    fn extent(self, width: u32, height: u32) -> (usize, usize) {
        let (width, height) = (width as usize, height as usize);
        match self {
            Self::Nv12 => (width, height + height.div_ceil(2)),
            Self::P010 => (2 * width, height + height.div_ceil(2)),
            Self::Yuv444P10 => (2 * width, 3 * height),
        }
    }

    fn format(self) -> NV_ENC_BUFFER_FORMAT {
        match self {
            Self::Nv12 => NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12,
            Self::P010 => NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_YUV420_10BIT,
            Self::Yuv444P10 => NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_YUV444_10BIT,
        }
    }
}

struct ChromaConvert {
    module: CUmodule,
    kernel: CUfunction,
    kernel_tex: CUfunction,
    layout: ConvertLayout,
    surface: CUdeviceptr,
    pitch: usize,
    registered: NV_ENC_REGISTERED_PTR,
    mapped: NV_ENC_INPUT_PTR,
}

impl ChromaConvert {
    /// JIT the module, allocate the `width`x`height` surface of `layout`, and register it with
    /// the session. `None` where any step refuses: the session then encodes the packed RGB itself,
    /// which sites chroma at the left of each block but converts with the matrix the session
    /// declares all the same.
    unsafe fn new(
        cuda: &CudaFunctions,
        funcs: &NV_ENCODE_API_FUNCTION_LIST,
        session: *mut c_void,
        width: u32,
        height: u32,
        layout: ConvertLayout,
    ) -> Option<Self> {
        let module = load_kernels(cuda)?;
        let mut kernel: CUfunction = ptr::null_mut();
        let mut kernel_tex: CUfunction = ptr::null_mut();
        let (linear, texture) = layout.kernels();
        if (cuda.cuModuleGetFunction)(&mut kernel, module, linear.as_ptr())
            != CUresult::CUDA_SUCCESS
            || (cuda.cuModuleGetFunction)(&mut kernel_tex, module, texture.as_ptr())
                != CUresult::CUDA_SUCCESS
        {
            (cuda.cuModuleUnload)(module);
            return None;
        }
        let (mut nv12, mut pitch): (CUdeviceptr, usize) = (0, 0);
        // The planes follow one another at the same pitch, which is the one allocation NVENC
        // reads all of. The chroma rows of a 4:2:0 layout round up, so the kernel's last row
        // is inside the allocation even at an odd height, which the capture paths do not
        // produce for a video codec but nothing here relies on.
        let (row_bytes, rows) = layout.extent(width, height);
        if (cuda.cuMemAllocPitch_v2)(&mut nv12, &mut pitch, row_bytes, rows, 4)
            != CUresult::CUDA_SUCCESS
        {
            (cuda.cuModuleUnload)(module);
            return None;
        }
        let mut reg = NV_ENC_REGISTER_RESOURCE {
            version: sv(NvStruct::RegisterResource),
            resourceType: NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR,
            width,
            height,
            resourceToRegister: nv12 as *mut c_void,
            pitch: pitch as u32,
            bufferFormat: layout.format(),
            bufferUsage: NV_ENC_BUFFER_USAGE::NV_ENC_INPUT_IMAGE,
            ..Default::default()
        };
        if (funcs.nvEncRegisterResource.unwrap())(session, &mut reg) != NVENCSTATUS::NV_ENC_SUCCESS
        {
            (cuda.cuMemFree_v2)(nv12);
            (cuda.cuModuleUnload)(module);
            return None;
        }
        let mut map = NV_ENC_MAP_INPUT_RESOURCE {
            version: sv(NvStruct::MapInputResource),
            registeredResource: reg.registeredResource,
            ..Default::default()
        };
        if (funcs.nvEncMapInputResource.unwrap())(session, &mut map) != NVENCSTATUS::NV_ENC_SUCCESS
        {
            (funcs.nvEncUnregisterResource.unwrap())(session, reg.registeredResource);
            (cuda.cuMemFree_v2)(nv12);
            (cuda.cuModuleUnload)(module);
            return None;
        }
        Some(ChromaConvert {
            module,
            kernel,
            kernel_tex,
            layout,
            surface: nv12,
            pitch,
            registered: reg.registeredResource,
            mapped: map.mappedResource,
        })
    }

    /// The registered surface and its format, as NVENC is handed them.
    fn input(&self) -> (NV_ENC_INPUT_PTR, NV_ENC_BUFFER_FORMAT) {
        (self.mapped, self.layout.format())
    }

    /// Convert the `width`x`height` packed surface at `src`/`src_pitch` into the surface,
    /// on the default stream so it is ordered behind the upload and ahead of the encode.
    /// `swap_rb` marks an RGBA byte order rather than BGRA.
    unsafe fn run(
        &self,
        cuda: &CudaFunctions,
        src: CUdeviceptr,
        src_pitch: usize,
        width: u32,
        height: u32,
        swap_rb: bool,
    ) -> Result<(), String> {
        let (mut src, mut sp) = (src, src_pitch as i32);
        let (mut dst, mut dp) = (self.surface, self.pitch as i32);
        let (mut w, mut h, mut swap) = (width as i32, height as i32, i32::from(swap_rb));
        let mut params: [*mut c_void; 7] = [
            &mut src as *mut _ as *mut c_void,
            &mut sp as *mut _ as *mut c_void,
            &mut dst as *mut _ as *mut c_void,
            &mut dp as *mut _ as *mut c_void,
            &mut w as *mut _ as *mut c_void,
            &mut h as *mut _ as *mut c_void,
            &mut swap as *mut _ as *mut c_void,
        ];
        self.launch(cuda, self.kernel, &mut params, width, height)
    }

    /// The same convert reading an array-typed import through `tex`, which leaves its RGB where
    /// the compositor put it instead of copying it into linear memory first.
    unsafe fn run_texture(
        &self,
        cuda: &CudaFunctions,
        tex: CUtexObject,
        width: u32,
        height: u32,
        swap_rb: bool,
    ) -> Result<(), String> {
        let mut tex = tex;
        let (mut dst, mut dp) = (self.surface, self.pitch as i32);
        let (mut w, mut h, mut swap) = (width as i32, height as i32, i32::from(swap_rb));
        let mut params: [*mut c_void; 6] = [
            &mut tex as *mut _ as *mut c_void,
            &mut dst as *mut _ as *mut c_void,
            &mut dp as *mut _ as *mut c_void,
            &mut w as *mut _ as *mut c_void,
            &mut h as *mut _ as *mut c_void,
            &mut swap as *mut _ as *mut c_void,
        ];
        self.launch(cuda, self.kernel_tex, &mut params, width, height)
    }

    /// One chroma sample per thread, on the default stream so the convert is ordered behind
    /// whatever produced the source and ahead of the encode.
    unsafe fn launch(
        &self,
        cuda: &CudaFunctions,
        kernel: CUfunction,
        params: &mut [*mut c_void],
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        const BLOCK: u32 = 16;
        let grid = (
            width.div_ceil(2).div_ceil(BLOCK),
            height.div_ceil(2).div_ceil(BLOCK),
        );
        if (cuda.cuLaunchKernel)(
            kernel,
            grid.0,
            grid.1,
            1,
            BLOCK,
            BLOCK,
            1,
            0,
            ptr::null_mut(),
            params.as_mut_ptr(),
            ptr::null_mut(),
        ) != CUresult::CUDA_SUCCESS
        {
            return Err("the chroma convert kernel failed to launch".into());
        }
        Ok(())
    }

    /// A point-sampled, byte-valued texture over an array-typed import, or zero where the driver
    /// refuses one — the frame then stays on NVENC's own conversion.
    unsafe fn texture_for(cuda: &CudaFunctions, array: CUarray) -> CUtexObject {
        let mut res: CUDA_RESOURCE_DESC = std::mem::zeroed();
        res.resType = CUresourcetype::CU_RESOURCE_TYPE_ARRAY;
        res.res.array.hArray = array;
        let mut sampling: CUDA_TEXTURE_DESC = std::mem::zeroed();
        sampling.addressMode = [CUaddress_mode::CU_TR_ADDRESS_MODE_CLAMP; 3];
        sampling.filterMode = CUfilter_mode::CU_TR_FILTER_MODE_POINT;
        sampling.flags = CU_TRSF_READ_AS_INTEGER;
        let mut tex: CUtexObject = 0;
        if (cuda.cuTexObjectCreate)(&mut tex, &res, &sampling, ptr::null())
            != CUresult::CUDA_SUCCESS
        {
            return 0;
        }
        tex
    }

    /// Inner handle before outer, as the rest of the teardown does.
    unsafe fn release(
        &self,
        cuda: &CudaFunctions,
        funcs: &NV_ENCODE_API_FUNCTION_LIST,
        session: *mut c_void,
    ) {
        (funcs.nvEncUnmapInputResource.unwrap())(session, self.mapped);
        (funcs.nvEncUnregisterResource.unwrap())(session, self.registered);
        (cuda.cuMemFree_v2)(self.surface);
        (cuda.cuModuleUnload)(self.module);
    }
}

/// A hash per band of rows of a packed 32-bit frame in video memory (`band_hash`), computed
/// where the frame lies and read back as one word a band and column segment.
struct BandHash {
    module: CUmodule,
    kernel: CUfunction,
    sums: CUdeviceptr,
    capacity: usize,
}

/// The threads of a `band_hash` block, as the kernel sizes its reduction, and the blocks a band
/// is split into across its columns, so a frame's few bands fill the GPU: a P100 hashed 1080p in
/// 0.08 ms this way and in 0.12 with a block a band, its 34 bands leaving 22 of the 56
/// multiprocessors idle.
const HASH_THREADS: u32 = 256;
const HASH_SEGMENTS: u32 = 4;

impl BandHash {
    unsafe fn new(cuda: &CudaFunctions) -> Option<Self> {
        let module = load_kernels(cuda)?;
        let mut kernel: CUfunction = ptr::null_mut();
        if (cuda.cuModuleGetFunction)(&mut kernel, module, c"band_hash".as_ptr())
            != CUresult::CUDA_SUCCESS
        {
            (cuda.cuModuleUnload)(module);
            return None;
        }
        Some(Self {
            module,
            kernel,
            sums: 0,
            capacity: 0,
        })
    }

    /// On the default stream so the hash is ordered behind whatever produced the frame; the copy
    /// back waits for it.
    unsafe fn run(
        &mut self,
        cuda: &CudaFunctions,
        src: CUdeviceptr,
        pitch: usize,
        width: u32,
        height: u32,
        rows: u32,
    ) -> Option<Vec<u64>> {
        let words = self.launch(cuda, src, pitch, width, height, rows)?;
        self.read(cuda, words)
    }

    /// The kernel alone, queued on the default stream behind whatever produced the frame: the
    /// words `read` copies back, or None where it did not launch.
    unsafe fn launch(
        &mut self,
        cuda: &CudaFunctions,
        src: CUdeviceptr,
        pitch: usize,
        width: u32,
        height: u32,
        rows: u32,
    ) -> Option<usize> {
        let bands = height.div_ceil(rows.max(1)) as usize;
        let words = bands * HASH_SEGMENTS as usize;
        if words > self.capacity {
            if self.sums != 0 {
                (cuda.cuMemFree_v2)(self.sums);
            }
            self.capacity = 0;
            if (cuda.cuMemAlloc_v2)(&mut self.sums, words * 8) != CUresult::CUDA_SUCCESS {
                self.sums = 0;
                return None;
            }
            self.capacity = words;
        }
        let (mut src, mut sp, mut out) = (src, pitch as i32, self.sums);
        let (mut w, mut h, mut r) = (width as i32, height as i32, rows.max(1) as i32);
        let mut params: [*mut c_void; 6] = [
            &mut src as *mut _ as *mut c_void,
            &mut sp as *mut _ as *mut c_void,
            &mut w as *mut _ as *mut c_void,
            &mut h as *mut _ as *mut c_void,
            &mut r as *mut _ as *mut c_void,
            &mut out as *mut _ as *mut c_void,
        ];
        ((cuda.cuLaunchKernel)(
            self.kernel,
            bands as u32,
            HASH_SEGMENTS,
            1,
            HASH_THREADS,
            1,
            1,
            0,
            ptr::null_mut(),
            params.as_mut_ptr(),
            ptr::null_mut(),
        ) == CUresult::CUDA_SUCCESS)
            .then_some(words)
    }

    /// The hashes a `launch` of `words` left, a word a band: the copy back waits for the kernel.
    unsafe fn read(&mut self, cuda: &CudaFunctions, words: usize) -> Option<Vec<u64>> {
        let mut sums = vec![0u64; words];
        ((cuda.cuMemcpyDtoH_v2)(sums.as_mut_ptr() as *mut c_void, self.sums, words * 8)
            == CUresult::CUDA_SUCCESS)
            .then(|| {
                sums.chunks(HASH_SEGMENTS as usize)
                    .map(|band| band.iter().fold(0u64, |a, &b| a.wrapping_add(b)))
                    .collect()
            })
    }

    unsafe fn release(self, cuda: &CudaFunctions) {
        if self.sums != 0 {
            (cuda.cuMemFree_v2)(self.sums);
        }
        (cuda.cuModuleUnload)(self.module);
    }
}

/// A live NVENC encoder session with its CUDA context and interop resources.
///
/// One instance owns a CUDA context bound to a specific GPU plus an NVENC session and everything
/// the two input paths need:
///
/// - **Packed path**: a pitched device buffer (`input_device_ptr` / `input_pitch`) registered and
///   mapped as the NVENC input (`registered_input_resource` / `mapped_input_buffer`) in the byte
///   order `input_format` names (re-registered in place when a source of the other order arrives),
///   fed either by a host→device upload or by the copy arm of the dmabuf path.
/// - **Zero-copy dmabuf path**: `dmabuf_cache` memoizes each fd's EGLImage → CUDA import, keyed by
///   fd but validated against the buffer's `DmaBufIdentity` so a recycled fd re-imports; an import
///   whose plane NVENC can take as it is — pitch-linear memory or a packed 8-bit CUDA array — is
///   registered in place (`DmaBufInput::Direct`) unless `direct_dmabuf` was switched off, anything
///   else is copied into the packed input each frame.
///
/// `bitstream_buffers` is a ring of `BITSTREAM_BUFFERS` output buffers (`current_buffer_idx`
/// cycles it).
/// `pinned_hosts` maps each page-locked host upload source's base pointer to its registered length,
/// with a `0` length recording a failed registration so that address is never re-pinned.
/// `codec` and `fullcolor` name the session's codec and negotiated chroma; `current_qp` tracks the
/// live ConstQP so a paint-over reconfigure is skipped when unchanged. `encode_config` and
/// `init_params` are retained so in-place reconfigure can resubmit them.
/// `omit_stripe_headers` drops the wire
/// header, and `node_index` is the effective CUDA device this session is bound to — a reuse across
/// captures that now targets a different device must rebuild rather than reconfigure.
pub struct NvencEncoder {
    encoder_session: *mut c_void,
    cuda_context: CUcontext,
    cuda_device: CUdevice,
    /// The GPU's marketing name, for the one line that says which device encodes.
    device_name: String,
    egl_display: EGLDisplay,
    codec: Codec,
    fullcolor: bool,
    /// The bits per sample the session codes.
    bit_depth: u32,
    /// Whether the session was opened asking for 4:4:4 and for 10 bits, which a device without
    /// them codes at 4:2:0 and 8: a resize keeps the session for that ask or for what it codes,
    /// either of which opens the same session again.
    asked: (bool, bool),
    width: u32,
    height: u32,
    current_qp: u32,
    /// A capped constant quantizer's peak (`peak_of`), and the rate control it opened with,
    /// which a release restores.
    peak: Option<PeakBucket>,
    open_rc: NV_ENC_RC_PARAMS,
    /// The mean quantizer, in the device's own domain, the last frame was coded at; 0 unknown.
    last_avg_qp: u32,
    /// The quality index the next frame is held at whatever the rate control
    /// (`hold_quantizer`), the band of it that quantizer covers, and whether the driver has
    /// refused one, said once.
    held_qp: Option<u32>,
    held_band: Option<(f64, f64)>,
    /// The `(sliceMode, sliceModeData)` of an HEVC session's frames outside a band frame's,
    /// which `set_band_layout` puts back.
    slice_layout: (u32, u32),
    hold_refused: bool,
    /// The bands held in recent frames, by the timestamp the device knows each frame by, and
    /// whether a frame that held none left some of them out of every prediction since the
    /// cleanup last asked (`take_lost_cleanup`).
    band_log: VecDeque<(u64, f64, f64)>,
    lost_cleanup: bool,
    /// The bytes the next frame may come to (`cap`), and where the last was taken back for coming
    /// out past them and coded again, how far its held band then reached, or 0 (`refit`).
    cap: Option<usize>,
    refit: Option<f64>,
    /// Bytes of the last held frame, and the QP delta map a held band is coded through.
    held_bytes: usize,
    qp_map: Vec<i8>,
    /// The blocks the next frame of the rate control's refines (`refine_blocks`), whether the
    /// session's rate control takes a QP delta map, and whether the driver refused one, said
    /// once, which ends refinement on the session.
    refinement: Option<crate::ledger::Refinement>,
    delta_mapped: bool,
    refine_refused: bool,
    /// The quality index the rate control last coded a frame at (held frames aside), from the
    /// driver's average quantizer.
    last_quality: Option<u32>,
    /// Bytes of the last frame the rate control coded, held frames aside.
    last_bytes: Option<usize>,
    encode_config: NV_ENC_CONFIG,
    init_params: NV_ENC_INITIALIZE_PARAMS,
    input_device_ptr: CUdeviceptr,
    input_pitch: usize,
    input_format: NV_ENC_BUFFER_FORMAT,
    registered_input_resource: NV_ENC_REGISTERED_PTR,
    mapped_input_buffer: NV_ENC_INPUT_PTR,
    bitstream_buffers: Vec<NV_ENC_OUTPUT_PTR>,
    current_buffer_idx: usize,
    dmabuf_cache: HashMap<i32, CachedDmaBuf>,
    external_inputs: Vec<ExternalInput>,
    pinned_hosts: HashMap<usize, usize>,
    cuda: Arc<CudaFunctions>,
    /// The EGL entry points a dmabuf import takes, loaded for a session given an EGL display.
    egl: Option<Arc<EglFunctions>>,
    _nvenc_lib: Arc<NvencLibrary>,
    nvenc_funcs: NV_ENCODE_API_FUNCTION_LIST,
    omit_stripe_headers: bool,
    node_index: i32,
    /// Resolved once at init from `PIXELFLUX_NVENC_PIN`: page-lock the host upload sources so the
    /// copy is a direct pinned DMA rather than a pageable copy staged through a bounce buffer.
    pin_uploads: bool,
    /// Resolved once at init from `PIXELFLUX_NVENC_DIRECT`: register pitch-linear dmabuf imports
    /// with NVENC in place instead of copying them into the packed input each frame.
    direct_dmabuf: bool,
    /// The 4:2:0 chroma convert, where the driver took the kernel and the session is not 4:4:4.
    /// `None` leaves NVENC's own conversion in place.
    csc: Option<ChromaConvert>,
    /// The decoded picture buffer the session declares, in frames, which a resize lowers where
    /// the new level admits fewer and never raises.
    dpb: u32,
    /// The device's NVENC engines, which `split_mode` reads at every geometry.
    engines: Option<i32>,
    /// The frames the decoder holds, so a lost one can be left out of the predictions and each
    /// frame can name what it predicts from; None where the device cannot invalidate a
    /// reference, and nothing is tracked.
    references: Option<ReferenceWindow>,
    last_reference: Reference,
    /// The H.264 stream's `frame_num` a byte wider, where the session tracks its references: the
    /// frame where the counter wraps, which a loss covering it costs a key frame for, comes once
    /// in 65536 frames instead of 256.
    frame_num: WideFrameNum,
    /// The X server's blit semaphore imported into this context (`blit_semaphore_fd`), which
    /// `encode_after_blit` queues each frame's wait on; null until one is made.
    blit_semaphore: CUexternalSemaphore,
    /// The band hash (`band_hashes`), loaded by the first frame that asks for one; `Some(None)`
    /// where the driver refused it.
    band_hash: Option<Option<BandHash>>,
    /// The bands of rows the next host upload is hashed in on the device (`hash_next_upload`),
    /// the words that hash left for the frame being coded, and its hashes once read back
    /// (`take_upload_hashes`).
    upload_hash: Option<u32>,
    upload_words: Option<usize>,
    upload_hashes: Option<Vec<u64>>,
}

unsafe impl Send for NvencEncoder {}

/// Release every GPU resource the session holds, in the one teardown order the drivers
/// tolerate, so nothing leaks and no still-referenced handle is ever freed out from under the
/// driver.
///
/// The whole sequence runs with the owning CUDA context pushed current, because the `cuMemFree` /
/// `cuGraphicsUnregisterResource` / `cuMemHostUnregister` calls each act on the *current* context —
/// pop it first and the frees silently do nothing, leaking device memory. Within that, resources
/// go inner-handle before the outer handle that owns it, since freeing an owner first orphans or
/// faults on what still points into it: unmap the packed input before unregistering it, free its
/// device buffer, destroy the bitstream buffers, and release every cached dmabuf
/// import (`release_dmabuf_import`: its NVENC mapping and registration, then the CUDA resource and
/// the EGLImage) — all session-owned — before the encoder session itself, and destroy that
/// session before releasing the device's primary CUDA context it was opened against (the retain is
/// refcounted, so the context lives until the last session on that device releases it). The
/// page-locked host sources are unpinned in the same pass, each only when its recorded length is
/// non-zero (a `0` marks a registration that failed and so was never pinned).
impl Drop for NvencEncoder {
    fn drop(&mut self) {
        unsafe {
            let _ = (self.cuda.cuCtxPushCurrent_v2)(self.cuda_context);

            self.unmap_external_inputs();
            if let Some(csc) = self.csc.take() {
                csc.release(&self.cuda, &self.nvenc_funcs, self.encoder_session);
            }
            if let Some(Some(hash)) = self.band_hash.take() {
                hash.release(&self.cuda);
            }
            if !self.mapped_input_buffer.is_null() {
                (self.nvenc_funcs.nvEncUnmapInputResource.unwrap())(
                    self.encoder_session,
                    self.mapped_input_buffer,
                );
            }
            if !self.registered_input_resource.is_null() {
                (self.nvenc_funcs.nvEncUnregisterResource.unwrap())(
                    self.encoder_session,
                    self.registered_input_resource,
                );
            }
            if self.input_device_ptr != 0 {
                (self.cuda.cuMemFree_v2)(self.input_device_ptr);
            }

            for &bs in &self.bitstream_buffers {
                (self.nvenc_funcs.nvEncDestroyBitstreamBuffer.unwrap())(self.encoder_session, bs);
            }

            let imports: Vec<CachedDmaBuf> = self.dmabuf_cache.drain().map(|(_, c)| c).collect();
            for cache in imports {
                self.release_dmabuf_import(cache);
            }

            for (base, len) in &self.pinned_hosts {
                if *len > 0 {
                    (self.cuda.cuMemHostUnregister)(*base as *mut c_void);
                }
            }

            if !self.blit_semaphore.is_null()
                && let Some(destroy) = self.cuda.cuDestroyExternalSemaphore
            {
                destroy(self.blit_semaphore);
            }

            if !self.encoder_session.is_null() {
                (self.nvenc_funcs.nvEncDestroyEncoder.unwrap())(self.encoder_session);
            }

            (self.cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
            (self.cuda.cuDevicePrimaryCtxRelease_v2)(self.cuda_device);
        }
    }
}

/// Hold one reference on `device`'s primary CUDA context for the life of the process, once a
/// session on it has opened. A capture restarted while a game fills the GPU's memory then
/// retains the context it had instead of creating one, which can fail there and leave the
/// stream on the CPU. The cost is the idle context's 106 MiB of VRAM on a device that has
/// encoded once, kept also while no session runs. Its own copy of the driver's entry points
/// keeps `libcuda` loaded after the last session's copy goes.
fn keep_primary_context(device: CUdevice) {
    static KEPT: std::sync::Mutex<Vec<CUdevice>> = std::sync::Mutex::new(Vec::new());
    static CUDA: std::sync::OnceLock<Option<CudaFunctions>> = std::sync::OnceLock::new();
    let mut kept = KEPT.lock().unwrap_or_else(|e| e.into_inner());
    if kept.contains(&device) {
        return;
    }
    let Some(cuda) = CUDA.get_or_init(|| NvencEncoder::load_cuda().ok()) else {
        return;
    };
    let mut context: CUcontext = ptr::null_mut();
    if unsafe { (cuda.cuDevicePrimaryCtxRetain)(&mut context, device) } == CUresult::CUDA_SUCCESS {
        kept.push(device);
    }
}

/// The level an NVENC session advertises for a `width` x `height` stream at `fps` carrying up
/// to `bitrate_bps` (`NV_ENC_LEVEL` shares each codec's own numbering: level_idc for H.264,
/// general_level_idc for HEVC, seq_level_idx for AV1).
///
/// H.264 and HEVC carry the current geometry's level, the lowest a decoder is asked to accept,
/// so a hardware decoder that gates on it -- older Apple and Intel fixed-function parts refuse
/// a stream whose SPS names a level above their ceiling even for a picture they could hold --
/// takes the stream. `reconfigure_resolution` re-declares the level with a forced IDR on every
/// resize; the frame rate is floored at `HEADROOM_FPS`, so a live rate change between the
/// common rates, which carries no IDR, never moves it. AV1 instead holds the resize headroom's
/// level: NVENC validates an AV1 session's level against `maxEncodeWidth` x `maxEncodeHeight`
/// at init and refuses one that cannot hold it, and an AV1 hardware decoder is recent enough to
/// take that level whatever the picture. The driver holds every codec's level to its bitrate
/// ceiling as well, refusing a CBR target past it as an invalid level, so a declared rate
/// raises the level to the first that admits it; `hevc_high_tier` names the HEVC tier the
/// session declares, whose ceiling is the one that applies. Driver 595.71.05 holds an AV1
/// level to two thirds of its Annex A MaxBitrate, `NVENC_AV1_RATE` (5.1 26.7 Mbit/s rather
/// than 40, on an RTX 4090 and an L4, whatever the frame rate or buffer), where 595.91.07 and
/// 615.71.09 hold Annex A's, so a session asks for the Annex A level first and, refused, for
/// the level `level_rate` weighs the rate to (`NVENC_AV1_WEIGHTED`). An HEVC picture is counted
/// in whole 32-pixel coding tree blocks, NVENC's, as the driver counts it: 1280x720 at 144 fps
/// is inside 4.1 by its own samples, and the driver refuses it as 4.1.
fn nvenc_level(
    codec: Codec,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    hevc_high_tier: bool,
) -> u32 {
    let fps = fps.max(HEADROOM_FPS);
    match codec {
        Codec::Av1 => av1_level(
            width.max(HEADROOM_WIDTH),
            height.max(HEADROOM_HEIGHT),
            fps,
            bitrate_bps,
        ),
        Codec::H265 => h265_level(
            width.next_multiple_of(32),
            height.next_multiple_of(32),
            fps,
            bitrate_bps,
            hevc_high_tier,
        ),
        _ => h264_level(width, height, fps, bitrate_bps),
    }
}

impl NvencEncoder {
    /// Resolve EGL at runtime rather than link against it, so one binary boots even on hosts
    /// without EGL — it is needed only by the zero-copy dmabuf path — and reach the
    /// `eglCreateImageKHR` / `eglDestroyImageKHR` entry points through `eglGetProcAddress` because
    /// they are KHR *extensions* the base `libEGL` is not obliged to export as plain symbols.
    /// Erroring when the library or either extension is missing lets the caller fall back to another
    /// encoder instead of crashing at the first dmabuf import.
    fn load_egl() -> Result<EglFunctions, String> {
        unsafe {
            let lib_name = "libEGL.so.1";
            let lib = Library::new(lib_name)
                .or_else(|_| Library::new("libEGL.so"))
                .map_err(|e| format!("Could not load EGL library: {}", e))?;

            let get_proc_addr_sym: Symbol<unsafe extern "C" fn(*const c_char) -> *mut c_void> = lib
                .get(b"eglGetProcAddress\0")
                .map_err(|e| format!("Missing symbol eglGetProcAddress: {}", e))?;

            let eglGetProcAddress = *get_proc_addr_sym;

            let load_extension = |name: &str| -> Result<*mut c_void, String> {
                let c_name = CString::new(name).unwrap();
                let addr = eglGetProcAddress(c_name.as_ptr());
                if addr.is_null() {
                    Err(format!("EGL Extension not found: {}", name))
                } else {
                    Ok(addr)
                }
            };

            let create_addr = load_extension("eglCreateImageKHR")?;
            let destroy_addr = load_extension("eglDestroyImageKHR")?;

            Ok(EglFunctions {
                _lib: lib,
                eglGetProcAddress,
                eglCreateImageKHR: std::mem::transmute::<*mut c_void, EglCreateImageKhrFn>(
                    create_addr,
                ),
                eglDestroyImageKHR: std::mem::transmute::<*mut c_void, EglDestroyImageKhrFn>(
                    destroy_addr,
                ),
            })
        }
    }

    /// Resolve the CUDA driver library (`libcuda.so.1`, or `nvcuda.dll` on Windows) at
    /// runtime so the crate links against no CUDA SDK and still runs wherever a driver is installed,
    /// binding every `cu*` entry point up front so the per-frame hot path is plain indirect calls
    /// with no repeated symbol lookups. A missing symbol errors with its name, turning an ABI
    /// mismatch into a legible message instead of a later null-pointer call.
    fn load_cuda() -> Result<CudaFunctions, String> {
        unsafe {
            let lib_name = if cfg!(windows) {
                "nvcuda.dll"
            } else {
                "libcuda.so.1"
            };
            let lib = Library::new(lib_name)
                .map_err(|e| format!("Could not load CUDA library ({}): {}", lib_name, e))?;

            macro_rules! load {
                ($lib:expr, $name:expr) => {
                    *$lib.get($name).map_err(|e| {
                        format!(
                            "Missing symbol {}: {}",
                            std::str::from_utf8($name).unwrap(),
                            e
                        )
                    })?
                };
            }

            Ok(CudaFunctions {
                cuInit: load!(lib, b"cuInit\0"),
                cuDeviceGet: load!(lib, b"cuDeviceGet\0"),
                cuDeviceGetByPCIBusId: load!(lib, b"cuDeviceGetByPCIBusId\0"),
                cuDevicePrimaryCtxRetain: load!(lib, b"cuDevicePrimaryCtxRetain\0"),
                cuCtxPushCurrent_v2: load!(lib, b"cuCtxPushCurrent_v2\0"),
                cuCtxPopCurrent_v2: load!(lib, b"cuCtxPopCurrent_v2\0"),
                cuDevicePrimaryCtxRelease_v2: load!(lib, b"cuDevicePrimaryCtxRelease_v2\0"),
                cuMemAlloc_v2: load!(lib, b"cuMemAlloc_v2\0"),
                cuMemAllocPitch_v2: load!(lib, b"cuMemAllocPitch_v2\0"),
                cuMemFree_v2: load!(lib, b"cuMemFree_v2\0"),
                cuMemcpyHtoD_v2: load!(lib, b"cuMemcpyHtoD_v2\0"),
                cuMemcpyDtoH_v2: load!(lib, b"cuMemcpyDtoH_v2\0"),
                cuMemcpy2D_v2: load!(lib, b"cuMemcpy2D_v2\0"),
                cuMemcpy2DAsync_v2: load!(lib, b"cuMemcpy2DAsync_v2\0"),
                cuStreamSynchronize: load!(lib, b"cuStreamSynchronize\0"),
                cuModuleLoadData: load!(lib, b"cuModuleLoadData\0"),
                cuModuleGetFunction: load!(lib, b"cuModuleGetFunction\0"),
                cuModuleUnload: load!(lib, b"cuModuleUnload\0"),
                cuTexObjectCreate: load!(lib, b"cuTexObjectCreate\0"),
                cuTexObjectDestroy: load!(lib, b"cuTexObjectDestroy\0"),
                cuLaunchKernel: load!(lib, b"cuLaunchKernel\0"),
                cuMemHostRegister_v2: load!(lib, b"cuMemHostRegister_v2\0"),
                cuMemHostUnregister: load!(lib, b"cuMemHostUnregister\0"),
                cuGraphicsEGLRegisterImage: load!(lib, b"cuGraphicsEGLRegisterImage\0"),
                cuGraphicsUnregisterResource: load!(lib, b"cuGraphicsUnregisterResource\0"),
                cuGraphicsResourceGetMappedEglFrame: load!(
                    lib,
                    b"cuGraphicsResourceGetMappedEglFrame\0"
                ),
                cuDeviceGetCount: load!(lib, b"cuDeviceGetCount\0"),
                cuDeviceGetName: load!(lib, b"cuDeviceGetName\0"),
                cuDeviceGetUuid: load!(lib, b"cuDeviceGetUuid\0"),
                cuGetErrorName: load!(lib, b"cuGetErrorName\0"),
                cuImportExternalSemaphore: lib.get(b"cuImportExternalSemaphore\0").ok().map(|s| *s),
                cuWaitExternalSemaphoresAsync: lib
                    .get(b"cuWaitExternalSemaphoresAsync\0")
                    .ok()
                    .map(|s| *s),
                cuDestroyExternalSemaphore: lib
                    .get(b"cuDestroyExternalSemaphore\0")
                    .ok()
                    .map(|s| *s),
                _lib: lib,
            })
        }
    }

    /// Resolve `libnvidia-encode` at runtime for the same reason as CUDA — no SDK to link,
    /// runs against whatever driver ships — binding `NvEncodeAPICreateInstance` as the sole entry
    /// point every later encode call is reached through. `NvEncodeAPIGetMaxSupportedVersion` is kept
    /// optional and bound only when present, because very old drivers lack it; negotiation then falls
    /// back to probing `create_instance` acceptance directly rather than failing to load.
    fn load_nvenc() -> Result<NvencLibrary, String> {
        unsafe {
            let lib_name = NVENC_DLL_NAME;
            let lib = Library::new(lib_name)
                .map_err(|e| format!("Could not load NVENC library ({}): {}", lib_name, e))?;

            let create_instance = *lib
                .get(NV_ENCODE_API_CREATE_INSTANCE_FN_NAME)
                .map_err(|e| e.to_string())?;
            let get_max_version = lib
                .get::<NvEncodeApiGetMaxSupportedVersionFn>(
                    NV_ENCODE_API_GET_MAX_SUPPORTED_VERSION_FN_NAME,
                )
                .map(|s| *s)
                .ok();
            Ok(NvencLibrary {
                create_instance,
                get_max_version,
                _lib: lib,
            })
        }
    }

    /// Turn a `CUresult` into the driver's own error name via `cuGetErrorName` so a failure
    /// logs something diagnosable (e.g. `CUDA_ERROR_OUT_OF_MEMORY`) instead of a bare integer,
    /// falling back to the numeric code only when the name is unavailable.
    unsafe fn get_error_string(cuda: &CudaFunctions, err: CUresult) -> String {
        let mut p_str: *const c_char = ptr::null();
        if (cuda.cuGetErrorName)(err, &mut p_str) == CUresult::CUDA_SUCCESS && !p_str.is_null() {
            CStr::from_ptr(p_str).to_string_lossy().into_owned()
        } else {
            format!("Unknown CUDA Error ({})", err.0)
        }
    }

    /// Log the CUDA devices CUDA can enumerate — a debug aid when session init fails to find
    /// or bind the expected GPU.
    unsafe fn probe_devices(cuda: &CudaFunctions) {
        let mut count = 0;
        if (cuda.cuDeviceGetCount)(&mut count) != CUresult::CUDA_SUCCESS {
            return;
        }
        crate::log::debug!("[NVENC] Found {} CUDA devices:", count);
        for i in 0..count {
            let mut dev = 0;
            (cuda.cuDeviceGet)(&mut dev, i);
            crate::log::debug!(
                "[NVENC]   Device {}: {}",
                i,
                Self::device_name_of(cuda, dev)
            );
        }
    }

    /// The name CUDA gives a device, or a placeholder when it will not say.
    unsafe fn device_name_of(cuda: &CudaFunctions, dev: CUdevice) -> String {
        let mut name_buf = [0 as c_char; 256];
        if (cuda.cuDeviceGetName)(name_buf.as_mut_ptr(), 256, dev) != CUresult::CUDA_SUCCESS {
            return format!("CUDA device {dev}");
        }
        CStr::from_ptr(name_buf.as_ptr())
            .to_string_lossy()
            .into_owned()
    }

    /// The PCI bus ID of the GPU behind `/dev/dri/renderD<128+index>`, read from the sysfs
    /// device symlink, so CUDA can bind to the same physical GPU the capture render node lives on.
    fn get_pci_bus_id(render_index: i32) -> Option<String> {
        let path = format!("/sys/class/drm/renderD{}/device", 128 + render_index);
        if let Ok(target) = std::fs::read_link(&path)
            && let Some(name) = target.file_name()
            && let Some(name_str) = name.to_str()
        {
            return Some(name_str.to_string());
        }
        None
    }

    /// Build a live NVENC session for the settings' codec: bind CUDA to the target GPU, open and
    /// configure the encoder, and allocate its input and output buffers.
    ///
    /// The sequence:
    ///
    /// 1. **Load and negotiate**: dlopen EGL / CUDA / NVENC, then `nvenc_negotiate` resolves the API
    ///    version against the driver (set-once) before any struct is version-tagged. The multi-GPU
    ///    `GET_ATTACHED_IDS` ioctl filter is installed after the NVIDIA libraries are loaded — so
    ///    their GOTs can be patched — and before `cuInit` enumerates devices (a no-op unless a host
    ///    GPU is hidden from this container). The three library `Arc`s are leaked once per process so
    ///    the resolved function pointers stay valid for the program's life.
    /// 2. **Bind the device**: `cuInit`, then bind by the render node's PCI bus ID
    ///    (`encode_node_index`, with auto `<0` meaning device 0), falling back to CUDA device 0, and
    ///    retain the device's primary CUDA context — shared and refcounted across every session on
    ///    that device rather than a fresh 100-300 MiB context each — pushing it current.
    /// 3. **Allocate input**: a pitched ARGB device buffer (`cuMemAllocPitch`, 16-byte element
    ///    alignment) that the chroma convert, or hardware CSC, turns into YUV.
    /// 4. **Open the session and query caps**: create the function-list instance, open the session
    ///    with the negotiated `apiVersion`, refuse a codec the device lists no engine for, and
    ///    query `nvEncGetEncodeCaps` so init degrades rather than fails — a 4:4:4 request on a GPU
    ///    or codec without it drops to 4:2:0, and a capture beyond the encoder's max dimensions
    ///    returns `Err` so the caller falls back to software. Then pull a preset config (P3,
    ///    ultra-low-latency); a failed preset lookup logs the driver's error string and proceeds
    ///    with the zeroed default rather than aborting.
    /// 5. **Configure the stream** (mutating the returned preset config, whose `version` word is
    ///    re-stamped while its embedded `rcParams` keeps the version the preset fill set): the
    ///    codec's 4:2:0 or 4:4:4 profile; CBR (two-pass quarter-resolution for tighter per-frame
    ///    rate adherence, VBV sizing, optional min/max QP clamps in the codec's quantizer domain)
    ///    or ConstQP; infinite GOP (`gopLength` / `idrPeriod` = `0xFFFFFFFF`); `zeroReorderDelay`
    ///    and, for H.264, a bitstream-restriction VUI (`max_num_reorder_frames=0`) so no-reorder
    ///    decoders don't buffer; an explicit level from `nvenc_level` pinned from frame 1 so the
    ///    level never bumps mid-stream; BT.709 primaries and transfer for the sRGB source, at
    ///    limited range, with the matrix whichever conversion the session uses produces;
    ///    a decoded picture buffer of as many frames as the level admits, so a frame a client
    ///    lost can be left out of the predictions (`invalidate_reference`) with earlier frames
    ///    still there to predict from;
    ///    repeated parameter sets on every key frame; H.264 CABAC; no AUD; one AV1 tile; strict
    ///    GOP target; and lookahead disabled for real-time latency.
    /// 6. **Initialize with resize headroom**: `maxEncodeWidth` / `maxEncodeHeight` are raised to
    ///    at least `HEADROOM_WIDTH` x `HEADROOM_HEIGHT` so `reconfigure_resolution` can grow in
    ///    place, but never past the driver's reported maximum; this costs a few hundred MiB of
    ///    device memory, so a failed init reopens the session and retries at the exact size
    ///    (in-place resize then falls back to a rebuild).
    /// 7. **Register, map, and buffer**: register and map the packed input surface (as `ARGB`;
    ///    `set_input_format` re-registers it for an RGBA source), and create a
    ///    4-deep ring of bitstream output buffers.
    ///
    /// Every failure after the CUDA allocation unwinds the resources created so far — buffers,
    /// session, context — before returning `Err`. EGL is only needed by the zero-copy dmabuf path,
    /// so callers on the host-ARGB path pass a null `egl_display`. The retained
    /// `init_params.encodeConfig` raw pointer is nulled before the struct is returned (it points at
    /// a local `config` about to move); the reconfigure paths repoint it at `self.encode_config`
    /// when they resubmit.
    pub fn new(settings: &CaptureSettings, egl_display: *const c_void) -> Result<Self, String> {
        Self::new_tuned(settings, egl_display, NvencTuning::default())
    }

    /// `new` with the preset, rate-control passes, and adaptive quantization named, for the
    /// tuning bench; production sessions take `NvencTuning::default`.
    pub(crate) fn new_tuned(
        settings: &CaptureSettings,
        egl_display: *const c_void,
        tuning: NvencTuning,
    ) -> Result<Self, String> {
        let codec = settings.codec;
        let codec_guid =
            codec_guid(codec).ok_or_else(|| format!("NVENC has no {} encoder", codec.display()))?;
        crate::log::debug!("[NVENC] Initializing {}...", codec.display());

        let egl = if egl_display.is_null() {
            None
        } else {
            Some(Arc::new(Self::load_egl()?))
        };
        let cuda = Arc::new(Self::load_cuda()?);
        let nvenc_lib = Arc::new(Self::load_nvenc()?);
        nvenc_negotiate(&nvenc_lib);

        crate::nvgpufilter::install();

        static LEAK_ONCE: std::sync::Once = std::sync::Once::new();
        LEAK_ONCE.call_once(|| {
            std::mem::forget(egl.clone());
            std::mem::forget(cuda.clone());
            std::mem::forget(nvenc_lib.clone());
        });

        unsafe {
            let res = (cuda.cuInit)(0);
            if res != CUresult::CUDA_SUCCESS {
                return Err(format!(
                    "Init CUDA failed: {}",
                    Self::get_error_string(&cuda, res)
                ));
            }

            Self::probe_devices(&cuda);

            let mut cu_device: CUdevice = 0;
            let mut device_found = false;

            if let Some(pci_bus_id) = Self::get_pci_bus_id(settings.encode_node_index.max(0)) {
                let c_pci_bus_id = CString::new(pci_bus_id.clone()).unwrap();
                if (cuda.cuDeviceGetByPCIBusId)(&mut cu_device, c_pci_bus_id.as_ptr())
                    == CUresult::CUDA_SUCCESS
                {
                    crate::log::debug!(
                        "[NVENC] Bound to CUDA device via PCI Bus ID: {}",
                        pci_bus_id
                    );
                    device_found = true;
                }
            }

            if !device_found {
                let res = (cuda.cuDeviceGet)(&mut cu_device, 0);
                if res != CUresult::CUDA_SUCCESS {
                    return Err("Failed to get default CUDA device".into());
                }
            }
            let device_name = Self::device_name_of(&cuda, cu_device);

            // One primary context per device, shared and refcounted across every session on that
            // device, rather than a fresh 100-300 MiB context each: a second display or a rebuild
            // retains the same context instead of allocating another. Retain does not make it
            // current, so it is pushed here to run the allocations below and left current for the
            // encode paths — matching the current-context state the removed cuCtxCreate produced.
            let mut cu_context: CUcontext = ptr::null_mut();
            let res = (cuda.cuDevicePrimaryCtxRetain)(&mut cu_context, cu_device);
            if res != CUresult::CUDA_SUCCESS {
                return Err("Failed to retain the device's primary CUDA context".into());
            }
            if (cuda.cuCtxPushCurrent_v2)(cu_context) != CUresult::CUDA_SUCCESS {
                (cuda.cuDevicePrimaryCtxRelease_v2)(cu_device);
                return Err("Failed to make the primary CUDA context current".into());
            }

            let width = settings.width as u32;
            let height = settings.height as u32;
            let mut input_device_ptr: CUdeviceptr = 0;
            let mut input_pitch: usize = 0;

            let res = (cuda.cuMemAllocPitch_v2)(
                &mut input_device_ptr,
                &mut input_pitch,
                (width * 4) as usize,
                height as usize,
                16,
            );
            if res != CUresult::CUDA_SUCCESS {
                (cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
                (cuda.cuDevicePrimaryCtxRelease_v2)(cu_device);
                return Err("Failed to allocate ARGB input buffer on GPU".into());
            }

            let mut function_list = NV_ENCODE_API_FUNCTION_LIST {
                version: sv(NvStruct::FunctionList),
                ..Default::default()
            };
            if (nvenc_lib.create_instance)(&mut function_list) != NVENCSTATUS::NV_ENC_SUCCESS {
                (cuda.cuMemFree_v2)(input_device_ptr);
                (cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
                (cuda.cuDevicePrimaryCtxRelease_v2)(cu_device);
                return Err("NvEncodeAPICreateInstance failed".into());
            }

            let mut session_params = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
                version: sv(NvStruct::OpenSessionExParams),
                deviceType: NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_CUDA,
                device: cu_context as *mut c_void,
                apiVersion: neg_api(),
                ..Default::default()
            };

            let mut encoder_session: *mut c_void = ptr::null_mut();
            let open_fn = function_list.nvEncOpenEncodeSessionEx.unwrap();
            let status = open_fn(&mut session_params, &mut encoder_session);
            if status != NVENCSTATUS::NV_ENC_SUCCESS {
                (cuda.cuMemFree_v2)(input_device_ptr);
                (cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
                (cuda.cuDevicePrimaryCtxRelease_v2)(cu_device);
                return Err(session_refusal(status));
            }

            // The device's engine list decides whether the codec exists here at all (AV1
            // arrived with Ada), and a refusal has to name that rather than fail an init.
            if !Self::device_encodes(&function_list, encoder_session, &codec_guid) {
                (function_list.nvEncDestroyEncoder.unwrap())(encoder_session);
                (cuda.cuMemFree_v2)(input_device_ptr);
                (cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
                (cuda.cuDevicePrimaryCtxRelease_v2)(cu_device);
                return Err(format!(
                    "this GPU's NVENC has no {} engine",
                    codec.display()
                ));
            }

            // Query caps so init degrades instead of failing opaquely: a 4:4:4 request on a GPU
            // or codec without it drops to 4:2:0, and a capture beyond the encoder's max
            // dimensions declines NVENC so the caller falls back to software.
            let caps_444 = if codec.fullcolor() {
                query_cap(
                    &function_list,
                    encoder_session,
                    codec_guid,
                    NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_YUV444_ENCODE,
                )
            } else {
                Some(0)
            };
            let caps_wmax = query_cap(
                &function_list,
                encoder_session,
                codec_guid,
                NV_ENC_CAPS::NV_ENC_CAPS_WIDTH_MAX,
            );
            let caps_hmax = query_cap(
                &function_list,
                encoder_session,
                codec_guid,
                NV_ENC_CAPS::NV_ENC_CAPS_HEIGHT_MAX,
            );
            let caps = decide_caps(
                settings.video_fullcolor,
                width as i32,
                height as i32,
                caps_444,
                caps_wmax,
                caps_hmax,
            );
            if let Some((mw, mh)) = caps.too_large {
                (function_list.nvEncDestroyEncoder.unwrap())(encoder_session);
                (cuda.cuMemFree_v2)(input_device_ptr);
                (cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
                (cuda.cuDevicePrimaryCtxRelease_v2)(cu_device);
                return Err(format!(
                    "NVENC maximum encode size {mw}x{mh} exceeded by {width}x{height}; using software"
                ));
            }
            if caps.downgraded_color && super::software_fullcolor(codec) {
                (function_list.nvEncDestroyEncoder.unwrap())(encoder_session);
                (cuda.cuMemFree_v2)(input_device_ptr);
                (cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
                (cuda.cuDevicePrimaryCtxRelease_v2)(cu_device);
                return Err(format!(
                    "this GPU's NVENC engine encodes no {} 4:4:4, which the software encoder does",
                    codec.display()
                ));
            }
            if caps.downgraded_color {
                eprintln!(
                    "[NVENC] {} 4:4:4 (YUV444) encoding unsupported on this GPU; encoding 4:2:0.",
                    codec.display()
                );
            }
            if tuning.temporal_aq
                && query_cap(
                    &function_list,
                    encoder_session,
                    codec_guid,
                    NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_TEMPORAL_AQ,
                ) == Some(0)
            {
                (function_list.nvEncDestroyEncoder.unwrap())(encoder_session);
                (cuda.cuMemFree_v2)(input_device_ptr);
                (cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
                (cuda.cuDevicePrimaryCtxRelease_v2)(cu_device);
                return Err(format!(
                    "{} temporal AQ is not supported by this GPU",
                    codec.display()
                ));
            }

            let is_444 = caps.fullcolor;
            let bit_depth = if settings.video_bit_depth >= 10
                && codes_ten_bit(&function_list, encoder_session, codec, codec_guid)
            {
                10
            } else {
                8
            };
            let invalidation = query_cap(
                &function_list,
                encoder_session,
                codec_guid,
                NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_REF_PIC_INVALIDATION,
            ) == Some(1);

            let mut config = NV_ENC_CONFIG {
                version: sv(NvStruct::Config),
                ..Default::default()
            };
            let mut preset_config = NV_ENC_PRESET_CONFIG {
                version: sv(NvStruct::PresetConfig),
                presetCfg: config,
                ..Default::default()
            };

            let get_preset_ex = function_list.nvEncGetEncodePresetConfigEx.unwrap();
            let preset_status = get_preset_ex(
                encoder_session,
                codec_guid,
                tuning.preset,
                NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                &mut preset_config,
            );
            if preset_status != NVENCSTATUS::NV_ENC_SUCCESS {
                eprintln!(
                    "[NVENC] nvEncGetEncodePresetConfigEx failed ({preset_status:?}): {}",
                    last_error(&function_list, encoder_session)
                );
            }

            config = preset_config.presetCfg;
            config.version = sv(NvStruct::Config);
            config.profileGUID = profile_guid(codec, is_444, bit_depth);
            if settings.video_cbr_mode {
                let bps = cbr_bps(settings);
                log_held_rate(settings, bps);
                config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR;
                config.rcParams.multiPass = if tuning.multipass
                    == NV_ENC_MULTI_PASS::NV_ENC_TWO_PASS_QUARTER_RESOLUTION
                    && NVENC_SINGLE_PASS.load(Ordering::Relaxed)
                {
                    NV_ENC_MULTI_PASS::NV_ENC_MULTI_PASS_DISABLED
                } else {
                    tuning.multipass
                };
                set_cbr_rate(&mut config.rcParams, bps, cbr_vbv(settings, bps));
                let lo = codec.hardware_quantizer_bound(Hardware::Nvenc, settings.video_min_qp);
                if lo > 0 {
                    config.rcParams.set_enableMinQP(1);
                    config.rcParams.minQP.qpInterP = lo;
                    config.rcParams.minQP.qpInterB = lo;
                    config.rcParams.minQP.qpIntra = lo;
                }
                let hi = codec.hardware_quantizer_bound(Hardware::Nvenc, settings.video_max_qp);
                if hi > 0 {
                    config.rcParams.set_enableMaxQP(1);
                    config.rcParams.maxQP.qpInterP = hi;
                    config.rcParams.maxQP.qpInterB = hi;
                    config.rcParams.maxQP.qpIntra = hi;
                }
            } else {
                let q = codec.hardware_quantizer(Hardware::Nvenc, settings.video_crf);
                config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CONSTQP;
                config.rcParams.constQP.qpInterP = q;
                config.rcParams.constQP.qpInterB = q;
                config.rcParams.constQP.qpIntra = q;
            }
            config.rcParams.set_enableAQ(tuning.spatial_aq as u32);
            config
                .rcParams
                .set_enableTemporalAQ(tuning.temporal_aq as u32);
            config.frameIntervalP = 1;
            config.gopLength = 0xFFFFFFFF;
            config.rcParams.set_zeroReorderDelay(1);
            config.rcParams.set_strictGOPTarget(1);
            config.rcParams.set_enableLookahead(0);
            config.rcParams.lookaheadDepth = 0;
            let rate = FrameRate::of(settings.target_fps);
            let level_at = |weighted| {
                nvenc_level(
                    codec,
                    width,
                    height,
                    rate.ceil(),
                    level_rate(codec, config.rcParams.maxBitRate as u64, weighted),
                    tuning.hevc_high_tier,
                )
            };
            let level = level_at(NVENC_AV1_WEIGHTED.load(Ordering::Relaxed));
            let weighted_level = level_at(true);
            let dpb = match codec {
                Codec::H265 => h265_dpb_frames(level, width, height),
                Codec::Av1 => AV1_REFERENCES,
                _ => h264_dpb_frames(level, width, height),
            };
            let dpb = if codec == Codec::Av1 {
                dpb
            } else {
                dpb.min(crate::encoders::reference_frames(settings))
            };
            let ltr = query_cap(
                &function_list,
                encoder_session,
                codec_guid,
                NV_ENC_CAPS::NV_ENC_CAPS_NUM_MAX_LTR_FRAMES,
            );
            let anchors = anchor_count(
                codec,
                nvenc_cur_ver().0,
                invalidation,
                ltr,
                dpb,
                settings.acknowledge_references,
            );
            Self::configure_codec(
                &mut config,
                codec,
                is_444,
                bit_depth,
                level,
                dpb,
                anchors,
                &tuning,
            );
            #[cfg(test)]
            if let Some(refs) = tuning.ref_l0 {
                match codec {
                    Codec::H265 => config.encodeCodecConfig.hevcConfig.numRefL0 = refs,
                    Codec::Av1 => config.encodeCodecConfig.av1Config.numFwdRefs = refs,
                    _ => config.encodeCodecConfig.h264Config.numRefL0 = refs,
                }
            }
            let engines = query_cap(
                &function_list,
                encoder_session,
                codec_guid,
                NV_ENC_CAPS::NV_ENC_CAPS_NUM_ENCODER_ENGINES,
            );
            let split = split_mode(codec, width, height, engines, nvenc_cur_ver());
            #[cfg(test)]
            let split = tuning.split.unwrap_or(split);

            let mut init_params = NV_ENC_INITIALIZE_PARAMS {
                version: sv(NvStruct::InitializeParams),
                encodeGUID: codec_guid,
                presetGUID: tuning.preset,
                tuningInfo: NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                encodeWidth: width,
                encodeHeight: height,
                darWidth: width,
                darHeight: height,
                frameRateNum: rate.num,
                frameRateDen: rate.den,
                enablePTD: 1,
                encodeConfig: &mut config,
                maxEncodeWidth: nvenc_headroom(width, HEADROOM_WIDTH, caps_wmax),
                maxEncodeHeight: nvenc_headroom(height, HEADROOM_HEIGHT, caps_hmax),
                ..Default::default()
            };
            init_params.set_splitEncodeMode(split as u32);

            let init_fn = function_list.nvEncInitializeEncoder.unwrap();
            let mut headroom_status =
                init_fn(encoder_session, &mut Negotiated::new(init_params).value);
            if headroom_status != NVENCSTATUS::NV_ENC_SUCCESS && weighted_level != level {
                // A driver holding AV1 to `NVENC_AV1_RATE` refuses the Annex A level as invalid,
                // and a refused encoder stays refused: the weighted level opens a session of its
                // own.
                (function_list.nvEncDestroyEncoder.unwrap())(encoder_session);
                encoder_session = ptr::null_mut();
                if open_fn(&mut session_params, &mut encoder_session) != NVENCSTATUS::NV_ENC_SUCCESS
                {
                    (cuda.cuMemFree_v2)(input_device_ptr);
                    (cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
                    (cuda.cuDevicePrimaryCtxRelease_v2)(cu_device);
                    return Err("Failed to reopen the AV1 session at the weighted level".into());
                }
                config.encodeCodecConfig.av1Config.level = weighted_level;
                init_params.encodeConfig = &mut config;
                headroom_status = init_fn(encoder_session, &mut Negotiated::new(init_params).value);
                if headroom_status == NVENCSTATUS::NV_ENC_SUCCESS {
                    NVENC_AV1_WEIGHTED.store(true, Ordering::Relaxed);
                    eprintln!(
                        "[NVENC] The driver refused AV1 level {level} at {} kbps; declaring {weighted_level}",
                        config.rcParams.maxBitRate / 1000
                    );
                }
            }
            if headroom_status == NVENCSTATUS::NV_ENC_ERR_INVALID_PARAM
                && config.rcParams.multiPass
                    == NV_ENC_MULTI_PASS::NV_ENC_TWO_PASS_QUARTER_RESOLUTION
            {
                (function_list.nvEncDestroyEncoder.unwrap())(encoder_session);
                encoder_session = ptr::null_mut();
                if open_fn(&mut session_params, &mut encoder_session) != NVENCSTATUS::NV_ENC_SUCCESS
                {
                    (cuda.cuMemFree_v2)(input_device_ptr);
                    (cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
                    (cuda.cuDevicePrimaryCtxRelease_v2)(cu_device);
                    return Err("Failed to reopen the session for a single pass".into());
                }
                config.rcParams.multiPass = NV_ENC_MULTI_PASS::NV_ENC_MULTI_PASS_DISABLED;
                init_params.encodeConfig = &mut config;
                headroom_status = init_fn(encoder_session, &mut Negotiated::new(init_params).value);
                if headroom_status == NVENCSTATUS::NV_ENC_SUCCESS
                    && !NVENC_SINGLE_PASS.swap(true, Ordering::Relaxed)
                {
                    eprintln!(
                        "[NVENC] The driver refused the quarter-resolution first pass; coding CBR in a single pass"
                    );
                }
            }
            if headroom_status != NVENCSTATUS::NV_ENC_SUCCESS {
                eprintln!(
                    "[NVENC] Init with {}x{} resize headroom failed ({headroom_status:?}): {}",
                    init_params.maxEncodeWidth,
                    init_params.maxEncodeHeight,
                    last_error(&function_list, encoder_session)
                );
                // An encoder the driver refused to initialize stays refused, so the retry at the
                // exact capture size needs a session of its own rather than this one.
                (function_list.nvEncDestroyEncoder.unwrap())(encoder_session);
                encoder_session = ptr::null_mut();
                let reopened = open_fn(&mut session_params, &mut encoder_session)
                    == NVENCSTATUS::NV_ENC_SUCCESS;
                init_params.maxEncodeWidth = width;
                init_params.maxEncodeHeight = height;
                init_params.encodeConfig = &mut config;
                let exact_status = if reopened {
                    init_fn(encoder_session, &mut Negotiated::new(init_params).value)
                } else {
                    NVENCSTATUS::NV_ENC_ERR_NO_ENCODE_DEVICE
                };
                if exact_status != NVENCSTATUS::NV_ENC_SUCCESS {
                    let detail = if reopened {
                        let d = last_error(&function_list, encoder_session);
                        (function_list.nvEncDestroyEncoder.unwrap())(encoder_session);
                        d
                    } else {
                        "could not reopen the session".to_string()
                    };
                    (cuda.cuMemFree_v2)(input_device_ptr);
                    (cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
                    (cuda.cuDevicePrimaryCtxRelease_v2)(cu_device);
                    return Err(format!(
                        "Failed to initialize {} encoder at {width}x{height} ({exact_status:?}): {detail}",
                        codec.display()
                    ));
                }
                eprintln!("[NVENC] Running without resize headroom.");
            }

            init_params.encodeConfig = ptr::null_mut();

            let mut reg_res = NV_ENC_REGISTER_RESOURCE {
                version: sv(NvStruct::RegisterResource),
                resourceType: NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR,
                width,
                height,
                resourceToRegister: input_device_ptr as *mut c_void,
                pitch: input_pitch as u32,
                bufferFormat: NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB,
                bufferUsage: NV_ENC_BUFFER_USAGE::NV_ENC_INPUT_IMAGE,
                ..Default::default()
            };

            let register_fn = function_list.nvEncRegisterResource.unwrap();
            if register_fn(encoder_session, &mut reg_res) != NVENCSTATUS::NV_ENC_SUCCESS {
                (function_list.nvEncDestroyEncoder.unwrap())(encoder_session);
                (cuda.cuMemFree_v2)(input_device_ptr);
                (cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
                (cuda.cuDevicePrimaryCtxRelease_v2)(cu_device);
                return Err("Failed to register input buffer".into());
            }

            let mut map_params = NV_ENC_MAP_INPUT_RESOURCE {
                version: sv(NvStruct::MapInputResource),
                registeredResource: reg_res.registeredResource,
                ..Default::default()
            };
            let map_fn = function_list.nvEncMapInputResource.unwrap();
            if map_fn(encoder_session, &mut map_params) != NVENCSTATUS::NV_ENC_SUCCESS {
                (function_list.nvEncUnregisterResource.unwrap())(
                    encoder_session,
                    reg_res.registeredResource,
                );
                (function_list.nvEncDestroyEncoder.unwrap())(encoder_session);
                (cuda.cuMemFree_v2)(input_device_ptr);
                (cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
                (cuda.cuDevicePrimaryCtxRelease_v2)(cu_device);
                return Err("Failed to map input buffer".into());
            }

            let mut bitstream_buffers = Vec::new();
            let create_bs_fn = function_list.nvEncCreateBitstreamBuffer.unwrap();
            for _ in 0..BITSTREAM_BUFFERS {
                let mut bitstream_params = NV_ENC_CREATE_BITSTREAM_BUFFER {
                    version: sv(NvStruct::CreateBitstreamBuffer),
                    ..Default::default()
                };
                if create_bs_fn(encoder_session, &mut bitstream_params)
                    != NVENCSTATUS::NV_ENC_SUCCESS
                {
                    for &bs in &bitstream_buffers {
                        (function_list.nvEncDestroyBitstreamBuffer.unwrap())(encoder_session, bs);
                    }
                    (function_list.nvEncUnmapInputResource.unwrap())(
                        encoder_session,
                        map_params.mappedResource,
                    );
                    (function_list.nvEncUnregisterResource.unwrap())(
                        encoder_session,
                        reg_res.registeredResource,
                    );
                    (function_list.nvEncDestroyEncoder.unwrap())(encoder_session);
                    (cuda.cuMemFree_v2)(input_device_ptr);
                    (cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
                    (cuda.cuDevicePrimaryCtxRelease_v2)(cu_device);
                    return Err("Failed to create bitstream buffer".into());
                }
                bitstream_buffers.push(bitstream_params.bitstreamBuffer);
            }

            // 8-bit 4:4:4 subsamples nothing and needs no kernel; a driver that refuses the
            // 4:2:0 one keeps NVENC's own conversion, which follows the declared matrix either
            // way. A 10-bit session was initialized for the 10-bit surface only the kernel
            // writes.
            let layout = ConvertLayout::of(is_444, bit_depth);
            #[cfg(test)]
            let layout = layout.filter(|_| !tuning.hardware_csc);
            let csc = layout.and_then(|layout| {
                ChromaConvert::new(
                    &cuda,
                    &function_list,
                    encoder_session,
                    width,
                    height,
                    layout,
                )
            });
            if bit_depth > 8 && csc.is_none() {
                for buffer in &bitstream_buffers {
                    (function_list.nvEncDestroyBitstreamBuffer.unwrap())(encoder_session, *buffer);
                }
                (function_list.nvEncUnmapInputResource.unwrap())(
                    encoder_session,
                    map_params.mappedResource,
                );
                (function_list.nvEncUnregisterResource.unwrap())(
                    encoder_session,
                    reg_res.registeredResource,
                );
                (function_list.nvEncDestroyEncoder.unwrap())(encoder_session);
                (cuda.cuMemFree_v2)(input_device_ptr);
                (cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
                (cuda.cuDevicePrimaryCtxRelease_v2)(cu_device);
                return Err("the driver took no 10-bit convert kernel".into());
            }
            keep_primary_context(cu_device);
            crate::log::debug!(
                "[NVENC] {} initialized (4:4:4 mode: {}, chroma convert: {}).",
                codec.display(),
                is_444,
                if csc.is_some() { "kernel" } else { "hardware" }
            );

            Ok(Self {
                encoder_session,
                cuda_context: cu_context,
                cuda_device: cu_device,
                device_name,
                egl_display: egl_display as EGLDisplay,
                codec,
                fullcolor: is_444,
                bit_depth,
                asked: (
                    settings.video_fullcolor && codec.fullcolor(),
                    settings.video_bit_depth >= 10,
                ),
                width,
                height,
                current_qp: codec.hardware_quantizer(Hardware::Nvenc, settings.video_crf),
                peak: peak_of(settings),
                open_rc: config.rcParams,
                last_avg_qp: 0,
                held_qp: None,
                held_band: None,
                slice_layout: tuning.slices.map_or((0, 0), |n| (SLICE_MODE_COUNT, n)),
                cap: None,
                refit: None,
                hold_refused: false,
                band_log: VecDeque::new(),
                lost_cleanup: false,
                held_bytes: 0,
                qp_map: Vec::new(),
                refinement: None,
                delta_mapped: false,
                refine_refused: false,
                last_quality: None,
                last_bytes: None,
                encode_config: config,
                init_params,
                input_device_ptr,
                input_pitch,
                input_format: NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB,
                registered_input_resource: reg_res.registeredResource,
                mapped_input_buffer: map_params.mappedResource,
                bitstream_buffers,
                current_buffer_idx: 0,
                dmabuf_cache: HashMap::new(),
                external_inputs: Vec::new(),
                pinned_hosts: HashMap::new(),
                cuda,
                egl,
                _nvenc_lib: nvenc_lib,
                nvenc_funcs: function_list,
                omit_stripe_headers: settings.omit_stripe_headers,
                node_index: settings.encode_node_index.max(0),
                pin_uploads: std::env::var("PIXELFLUX_NVENC_PIN").as_deref() != Ok("0"),
                direct_dmabuf: std::env::var("PIXELFLUX_NVENC_DIRECT").as_deref() != Ok("0"),
                csc,
                dpb,
                engines,
                references: invalidation.then(|| {
                    if anchors > 0 {
                        let mut w =
                            ReferenceWindow::with_anchors(window_frames(codec, dpb, true), anchors);
                        if settings.acknowledge_references {
                            w.set_acknowledged();
                        }
                        if codec == Codec::Av1 {
                            w.lend_free_anchors();
                        }
                        w
                    } else {
                        ReferenceWindow::new(dpb)
                    }
                }),
                last_reference: Reference::Untracked,
                frame_num: WideFrameNum::default(),
                blit_semaphore: ptr::null_mut(),
                band_hash: None,
                upload_hash: None,
                upload_words: None,
                upload_hashes: None,
            })
        }
    }

    /// Whether the open session's device lists an encode engine for `codec`.
    unsafe fn device_encodes(
        funcs: &NV_ENCODE_API_FUNCTION_LIST,
        session: *mut c_void,
        codec: &GUID,
    ) -> bool {
        let (Some(count_fn), Some(list_fn)) =
            (funcs.nvEncGetEncodeGUIDCount, funcs.nvEncGetEncodeGUIDs)
        else {
            return true;
        };
        let mut count = 0u32;
        if count_fn(session, &mut count) != NVENCSTATUS::NV_ENC_SUCCESS || count == 0 {
            return false;
        }
        let mut guids = vec![GUID::default(); count as usize];
        let mut listed = 0u32;
        if list_fn(session, guids.as_mut_ptr(), count, &mut listed) != NVENCSTATUS::NV_ENC_SUCCESS {
            return false;
        }
        guids
            .iter()
            .take(listed as usize)
            .any(|g| guid_eq(g, codec))
    }

    /// Program the codec-specific arm of `config`: `level`, an infinite IDR period, the chroma
    /// format, 8-bit input and output, parameter sets repeated on every key frame, and the
    /// color description every session converts with.
    ///
    /// The whole description is BT.709 at limited range. Primaries and transfer describe the
    /// source, sRGB desktop pixels, which shares both with BT.709; the matrix is what
    /// `ChromaConvert` produces, and NVENC's own conversion — the fallback, and the 4:4:4
    /// sessions — follows the matrix declared here at the limited range it emits, which
    /// `gpu_hardware_conversion_matches_the_declared_matrix` holds it to. H.264 additionally
    /// restricts reordering in its VUI so no-reorder decoders don't buffer, and codes CABAC.
    /// H.264 frames carry `SLICES_PER_FRAME` slices and HEVC frames one, an HEVC band frame its
    /// own (`set_band_layout`), and an HEVC session filters across no slice boundary, which is
    /// what keeps a band's slice from softening the rows refined before it; AV1 asks for one tile, since tiles
    /// cost bitrate and buy no quality, and tier 0, the only tier NVENC takes for it. The driver
    /// honors a 1x1 request below 1986 pixels of picture height; above that it forces a second
    /// tile row whatever is asked for, in the low-latency presets this one is among but not from
    /// P5 up, so a 4K AV1 session codes two tile rows.
    ///
    /// HEVC declares the tier `h265_tier` names for its level, High: NVENC validates a CBR target
    /// against the MaxBR of the pinned level, and the Main-tier ceiling of the 5.x levels
    /// (40 Mbit/s at 5.1) is one a 4K desktop session reaches, where a Main-tier open is refused
    /// and a live rate change past it is declined. The tier is a signaled cap, not a coding
    /// tool; it does change the codec string a client derives from the SPS (`H153` for `L153`).
    #[allow(clippy::too_many_arguments)]
    fn configure_codec(
        config: &mut NV_ENC_CONFIG,
        codec: Codec,
        fullcolor: bool,
        bit_depth: u32,
        level: u32,
        dpb: u32,
        anchors: usize,
        tuning: &NvencTuning,
    ) {
        let primaries = NV_ENC_VUI_COLOR_PRIMARIES::NV_ENC_VUI_COLOR_PRIMARIES_BT709;
        let transfer = NV_ENC_VUI_TRANSFER_CHARACTERISTIC::NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
        let matrix = NV_ENC_VUI_MATRIX_COEFFS::NV_ENC_VUI_MATRIX_COEFFS_BT709;
        let vui = |vui: &mut NV_ENC_CONFIG_H264_VUI_PARAMETERS| {
            vui.videoSignalTypePresentFlag = 1;
            vui.videoFormat = NV_ENC_VUI_VIDEO_FORMAT::NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
            vui.colourDescriptionPresentFlag = 1;
            vui.colourPrimaries = primaries;
            vui.transferCharacteristics = transfer;
            vui.colourMatrix = matrix;
            vui.videoFullRangeFlag = 0;
        };
        unsafe {
            match codec {
                Codec::H265 => {
                    let c = &mut config.encodeCodecConfig.hevcConfig;
                    c.level = level;
                    c.maxNumRefFramesInDPB = dpb;
                    if anchors > 0 {
                        c.set_enableLTR(1);
                        c.ltrTrustMode = 0;
                        c.ltrNumFrames = anchors as u32;
                    }
                    c.tier = if tuning.hevc_high_tier {
                        h265_tier(level)
                    } else {
                        0
                    };
                    c.set_disableDeblockAcrossSliceBoundary(1);
                    (c.sliceMode, c.sliceModeData) =
                        tuning.slices.map_or((0, 0), |n| (SLICE_MODE_COUNT, n));
                    c.idrPeriod = 0xFFFFFFFF;
                    c.set_chromaFormatIDC(if fullcolor { 3 } else { 1 });
                    c.set_repeatSPSPPS(1);
                    c.set_outputAUD(0);
                    vui(&mut c.hevcVUIParameters);
                }
                Codec::Av1 => {
                    let c = &mut config.encodeCodecConfig.av1Config;
                    c.level = level;
                    c.tier = 0;
                    c.idrPeriod = 0xFFFFFFFF;
                    c.set_chromaFormatIDC(1);
                    c.maxNumRefFramesInDPB = dpb;
                    if anchors > 0 {
                        c.set_enableLTR(1);
                        c.ltrNumFrames = anchors as u32;
                    }
                    c.set_repeatSeqHdr(1);
                    c.set_outputAnnexBFormat(0);
                    c.set_enableBitstreamPadding(0);
                    c.numTileColumns = 1;
                    c.numTileRows = 1;
                    c.colorPrimaries = primaries;
                    c.transferCharacteristics = transfer;
                    c.matrixCoefficients = matrix;
                    c.colorRange = 0;
                }
                _ => {
                    let c = &mut config.encodeCodecConfig.h264Config;
                    c.level = level;
                    c.maxNumRefFrames = dpb;
                    if anchors > 0 {
                        c.set_enableLTR(1);
                        c.ltrTrustMode = 0;
                        c.ltrNumFrames = anchors as u32;
                    }
                    c.sliceMode = SLICE_MODE_COUNT;
                    c.sliceModeData = tuning.slices.unwrap_or(SLICES_PER_FRAME);
                    c.idrPeriod = 0xFFFFFFFF;
                    c.chromaFormatIDC = if fullcolor { 3 } else { 1 };
                    c.set_repeatSPSPPS(1);
                    c.entropyCodingMode =
                        NV_ENC_H264_ENTROPY_CODING_MODE::NV_ENC_H264_ENTROPY_CODING_MODE_CABAC;
                    c.set_outputAUD(0);
                    c.h264VUIParameters.bitstreamRestrictionFlag = 1;
                    vui(&mut c.h264VUIParameters);
                }
            }
        }
        set_bit_depth(config, codec, bit_depth, nvenc_cur_ver());
    }

    /// The level the live config declares, read from its codec arm.
    fn declared_level(&self) -> u32 {
        let config = &self.encode_config.encodeCodecConfig;
        unsafe {
            match self.codec {
                Codec::H265 => config.hevcConfig.level,
                Codec::Av1 => config.av1Config.level,
                _ => config.h264Config.level,
            }
        }
    }

    /// The level a `width` x `height` stream at `fps` takes at the live config's peak bitrate
    /// and HEVC tier, and the driver's AV1 share (`NVENC_AV1_WEIGHTED`).
    fn level_for(&self, width: u32, height: u32, fps: u32) -> u32 {
        let high_tier = unsafe { self.encode_config.encodeCodecConfig.hevcConfig.tier == 1 };
        nvenc_level(
            self.codec,
            width,
            height,
            fps,
            level_rate(
                self.codec,
                self.encode_config.rcParams.maxBitRate as u64,
                NVENC_AV1_WEIGHTED.load(Ordering::Relaxed),
            ),
            high_tier,
        )
    }

    /// Write the level for a new geometry or frame rate into the live config's codec arm.
    fn set_level(&mut self, width: u32, height: u32, fps: u32) {
        let level = self.level_for(width, height, fps);
        match self.codec {
            Codec::H265 => self.encode_config.encodeCodecConfig.hevcConfig.level = level,
            Codec::Av1 => self.encode_config.encodeCodecConfig.av1Config.level = level,
            _ => self.encode_config.encodeCodecConfig.h264Config.level = level,
        }
    }

    /// The decoded picture buffer, in frames, the level a `width` x `height` stream at `fps`
    /// takes admits for this codec.
    fn dpb_frames_at(&self, width: u32, height: u32, fps: u32) -> u32 {
        let level = self.level_for(width, height, fps);
        match self.codec {
            Codec::H265 => h265_dpb_frames(level, width, height),
            Codec::Av1 => AV1_REFERENCES,
            _ => h264_dpb_frames(level, width, height),
        }
    }

    /// The frame the last encoded frame predicted from.
    pub fn last_reference(&self) -> Reference {
        self.last_reference
    }

    /// Leave frame `frame_id` and every frame after it out of the predictions. False when the
    /// device cannot or refuses, and the caller codes a key frame instead.
    pub fn invalidate_reference(&mut self, frame_id: u16) -> bool {
        let Some(references) = &mut self.references else {
            return false;
        };
        match references.invalidate(frame_id) {
            Invalidation::Forget(pts) => unsafe {
                (self.nvenc_funcs.nvEncInvalidateRefFrames.unwrap())(self.encoder_session, pts)
                    == NVENCSTATUS::NV_ENC_SUCCESS
            },
            Invalidation::KeyFrame | Invalidation::Ignored => true,
        }
    }

    /// Every consumer holds frame `frame_id`, or where not `held` was sent it
    /// (`ReferenceWindow::acknowledge`).
    pub fn acknowledge_reference(&mut self, frame_id: u16, held: bool) {
        if let Some(references) = &mut self.references {
            references.acknowledge(frame_id, held);
        }
    }

    /// Apply what the consumers say of a frame (`FrameEncoder::take_report`); false where a key
    /// frame is coded instead.
    pub fn take_report(&mut self, report: ReferenceReport) -> bool {
        match report {
            ReferenceReport::Lost(frame_id) | ReferenceReport::Withheld(frame_id) => {
                // The cleanup starts again for a loss (`StripeState::lost_frame`).
                self.band_log.clear();
                self.invalidate_reference(frame_id)
            }
            ReferenceReport::Held(frame_id) => {
                self.acknowledge_reference(frame_id, true);
                true
            }
            ReferenceReport::Sent(frame_id) => {
                self.acknowledge_reference(frame_id, false);
                true
            }
        }
    }

    /// Leave the frame just encoded out of the predictions as a frame the client lost is, for a
    /// frame coded again in its place (`ReferenceWindow::retract`); false where the window or
    /// the device cannot.
    fn retract_last(&mut self) -> bool {
        let (funcs, session) = (&self.nvenc_funcs, self.encoder_session);
        self.references.as_mut().is_some_and(|r| {
            r.retract(|pts| unsafe {
                (funcs.nvEncInvalidateRefFrames.unwrap())(session, pts)
                    == NVENCSTATUS::NV_ENC_SUCCESS
            })
        })
    }

    /// How the session splits a frame across the device's encode engines (`split_mode`), for the
    /// line that says which device encodes.
    pub fn split_summary(&self) -> String {
        let forced = [
            NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_AUTO_FORCED_MODE,
            NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_THREE_FORCED_MODE,
        ]
        .map(|m| m as u32)
        .contains(&self.init_params.splitEncodeMode());
        match self.engines {
            Some(n) if n > 1 && forced => format!("split across {n} engines"),
            Some(n) if n > 1 && self.codec != Codec::H264 => {
                format!("split as the driver picks, {n} engines")
            }
            Some(n) => format!("{n} engine{}", if n == 1 { "" } else { "s" }),
            None => "engines unreported".into(),
        }
    }

    /// The codec the session emits.
    /// The name CUDA gives the GPU this session encodes on.
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    pub fn codec(&self) -> Codec {
        self.codec
    }

    /// Every NVENC session converts to and declares limited range.
    pub fn is_full_range(&self) -> bool {
        false
    }

    /// Whether the session negotiated 4:4:4 chroma.
    pub fn is_fullcolor(&self) -> bool {
        self.fullcolor
    }

    /// The bits per sample the session codes.
    pub fn bit_depth(&self) -> u32 {
        self.bit_depth
    }

    /// Follow a capture restart on the live session, folding in the current rate / QP / fps,
    /// without tearing it down.
    ///
    /// The NVENC session, CUDA context, and bitstream buffers survive, so a restart costs a few
    /// milliseconds instead of a full rebuild. Flow:
    ///
    /// 1. **Reject the unchangeable**: a different encode device or codec, a flip of the chroma
    ///    format or depth asked for (4:4:4, 10-bit; not of what a device without them codes), an
    ///    RC-mode flip, or dimensions of zero or beyond the init-time `maxEncode` headroom all
    ///    return `Err` so the caller rebuilds.
    /// 2. **Keep the stream at unchanged dimensions**: the reference chain, the input surface, and
    ///    the dmabuf imports stay as they are, so the restart costs no IDR and no reset. Only the
    ///    pinned hosts are dropped -- the restart recreates the source buffers, often at the same
    ///    addresses -- and the rate, frame rate, and wire framing the restart carries are folded
    ///    in, as `reconfigure_rate` does. Returns `Ok(false)`.
    /// 3. **Release geometry-dependent state** under the pushed CUDA context: unmap / unregister /
    ///    free the packed input surface, every cached dmabuf import (with the NVENC registration a
    ///    direct import holds), and every pinned host. The dmabuf imports are re-created lazily by
    ///    the encode path; pinned hosts are dropped because the source shm segments are recreated
    ///    on resize and may reuse the same base addresses.
    /// 4. **Reconfigure the session**: update the level for the new size, the decoded picture
    ///    buffer where that level admits fewer frames (the driver takes a smaller one at the
    ///    forced IDR and refuses to raise it again), the CBR bitrate + VBV or the ConstQP, and the
    ///    new dimensions / DAR / frame rate, then `NvEncReconfigureEncoder` with `resetEncoder`
    ///    and `forceIDR` so the stream restarts cleanly at the new size. Driver rejection returns
    ///    `Err`.
    /// 5. **Reallocate the packed input** at the new size and register + map it as init does, in
    ///    the byte order the session was last fed.
    ///
    /// On a resize the next encoded frame is a reset-RC IDR and `Ok(true)` is returned.
    pub fn reconfigure_resolution(&mut self, settings: &CaptureSettings) -> Result<bool, String> {
        let new_w = settings.width as u32;
        let new_h = settings.height as u32;
        let is_cbr = self.encode_config.rcParams.rateControlMode
            == NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR;
        if settings.encode_node_index.max(0) != self.node_index {
            return Err("encode device changed".into());
        }
        if settings.codec != self.codec {
            return Err("codec changed".into());
        }
        let fullcolor = settings.video_fullcolor && self.codec.fullcolor();
        if fullcolor != self.asked.0 && fullcolor != self.fullcolor {
            return Err("chroma format changed".into());
        }
        let ten_bit = settings.video_bit_depth >= 10;
        if ten_bit != self.asked.1
            && ten_bit != (self.bit_depth > 8)
            && matches!(self.codec, Codec::H265 | Codec::Av1)
        {
            return Err("bit depth changed".into());
        }
        if settings.video_cbr_mode != is_cbr {
            return Err("rate-control mode changed".into());
        }
        if new_w == 0
            || new_h == 0
            || new_w > self.init_params.maxEncodeWidth
            || new_h > self.init_params.maxEncodeHeight
        {
            return Err(format!(
                "{}x{} outside reconfigure headroom {}x{}",
                new_w, new_h, self.init_params.maxEncodeWidth, self.init_params.maxEncodeHeight
            ));
        }
        if (new_w, new_h) == (self.width, self.height) {
            self.release_pinned_hosts();
            self.reconfigure_rate(settings);
            self.omit_stripe_headers = settings.omit_stripe_headers;
            return Ok(false);
        }
        let rate = FrameRate::of(settings.target_fps);
        let dpb = self.dpb_frames_at(new_w, new_h, rate.ceil()).min(self.dpb);

        unsafe {
            let _ = (self.cuda.cuCtxPushCurrent_v2)(self.cuda_context);
            self.unmap_external_inputs();
            if let Some(csc) = self.csc.take() {
                csc.release(&self.cuda, &self.nvenc_funcs, self.encoder_session);
            }
            if !self.mapped_input_buffer.is_null() {
                (self.nvenc_funcs.nvEncUnmapInputResource.unwrap())(
                    self.encoder_session,
                    self.mapped_input_buffer,
                );
                self.mapped_input_buffer = ptr::null_mut();
            }
            if !self.registered_input_resource.is_null() {
                (self.nvenc_funcs.nvEncUnregisterResource.unwrap())(
                    self.encoder_session,
                    self.registered_input_resource,
                );
                self.registered_input_resource = ptr::null_mut();
            }
            if self.input_device_ptr != 0 {
                (self.cuda.cuMemFree_v2)(self.input_device_ptr);
                self.input_device_ptr = 0;
            }
            let imports: Vec<CachedDmaBuf> = self.dmabuf_cache.drain().map(|(_, c)| c).collect();
            for cache in imports {
                self.release_dmabuf_import(cache);
            }
            for (base, len) in self.pinned_hosts.drain() {
                if len > 0 {
                    (self.cuda.cuMemHostUnregister)(base as *mut c_void);
                }
            }

            if is_cbr {
                let bps = cbr_bps(settings);
                set_cbr_rate(
                    &mut self.encode_config.rcParams,
                    bps,
                    cbr_vbv(settings, bps),
                );
            } else {
                let qp = self
                    .codec
                    .hardware_quantizer(Hardware::Nvenc, settings.video_crf);
                self.encode_config.rcParams.constQP.qpInterP = qp;
                self.encode_config.rcParams.constQP.qpInterB = qp;
                self.encode_config.rcParams.constQP.qpIntra = qp;
                self.current_qp = qp;
            }
            self.set_level(new_w, new_h, rate.ceil());
            match self.codec {
                Codec::H265 => {
                    self.encode_config
                        .encodeCodecConfig
                        .hevcConfig
                        .maxNumRefFramesInDPB = dpb
                }
                Codec::Av1 => {
                    self.encode_config
                        .encodeCodecConfig
                        .av1Config
                        .maxNumRefFramesInDPB = dpb
                }
                _ => {
                    self.encode_config
                        .encodeCodecConfig
                        .h264Config
                        .maxNumRefFrames = dpb
                }
            }
            self.init_params.encodeWidth = new_w;
            self.init_params.encodeHeight = new_h;
            self.init_params.darWidth = new_w;
            self.init_params.darHeight = new_h;
            self.init_params.frameRateNum = rate.num;
            self.init_params.frameRateDen = rate.den;
            self.init_params.set_splitEncodeMode(split_mode(
                self.codec,
                new_w,
                new_h,
                self.engines,
                nvenc_cur_ver(),
            ) as u32);
            if self.reconfigure(true, true) != NVENCSTATUS::NV_ENC_SUCCESS {
                (self.cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
                return Err("NvEncReconfigureEncoder rejected the resolution change".into());
            }
            self.width = new_w;
            self.height = new_h;
            self.dpb = dpb;
            if let Some(references) = &mut self.references {
                references.set_capacity(window_frames(self.codec, dpb, references.anchored()));
                references.reset();
            }

            let mut input_device_ptr: CUdeviceptr = 0;
            let mut input_pitch: usize = 0;
            let res = (self.cuda.cuMemAllocPitch_v2)(
                &mut input_device_ptr,
                &mut input_pitch,
                (new_w * 4) as usize,
                new_h as usize,
                16,
            );
            if res != CUresult::CUDA_SUCCESS {
                (self.cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
                return Err("Failed to allocate ARGB input buffer on GPU".into());
            }
            let mut reg_res = NV_ENC_REGISTER_RESOURCE {
                version: sv(NvStruct::RegisterResource),
                resourceType: NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR,
                width: new_w,
                height: new_h,
                resourceToRegister: input_device_ptr as *mut c_void,
                pitch: input_pitch as u32,
                bufferFormat: self.input_format,
                bufferUsage: NV_ENC_BUFFER_USAGE::NV_ENC_INPUT_IMAGE,
                ..Default::default()
            };
            let register_fn = self.nvenc_funcs.nvEncRegisterResource.unwrap();
            if register_fn(self.encoder_session, &mut reg_res) != NVENCSTATUS::NV_ENC_SUCCESS {
                (self.cuda.cuMemFree_v2)(input_device_ptr);
                (self.cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
                return Err("Failed to register input buffer".into());
            }
            let mut map_params = NV_ENC_MAP_INPUT_RESOURCE {
                version: sv(NvStruct::MapInputResource),
                registeredResource: reg_res.registeredResource,
                ..Default::default()
            };
            let map_fn = self.nvenc_funcs.nvEncMapInputResource.unwrap();
            if map_fn(self.encoder_session, &mut map_params) != NVENCSTATUS::NV_ENC_SUCCESS {
                (self.nvenc_funcs.nvEncUnregisterResource.unwrap())(
                    self.encoder_session,
                    reg_res.registeredResource,
                );
                (self.cuda.cuMemFree_v2)(input_device_ptr);
                (self.cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
                return Err("Failed to map input buffer".into());
            }
            self.input_device_ptr = input_device_ptr;
            self.input_pitch = input_pitch;
            self.registered_input_resource = reg_res.registeredResource;
            self.mapped_input_buffer = map_params.mappedResource;
            if let Some(layout) = ConvertLayout::of(self.fullcolor, self.bit_depth) {
                self.csc = ChromaConvert::new(
                    &self.cuda,
                    &self.nvenc_funcs,
                    self.encoder_session,
                    new_w,
                    new_h,
                    layout,
                );
                if self.bit_depth > 8 && self.csc.is_none() {
                    (self.cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
                    return Err("the driver took no 10-bit convert kernel".into());
                }
            }
            (self.cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
        }
        self.omit_stripe_headers = settings.omit_stripe_headers;
        Ok(true)
    }

    /// Page-lock one host upload source's base address once, under the already-current CUDA
    /// context, so the copy is a direct pinned DMA instead of a pageable copy staged through a driver
    /// bounce buffer. A `0`-length entry records a failed registration so the address is never
    /// re-probed; the persistent, bounded shm / reused planar sources make this a one-time cost.
    unsafe fn pin_host_source(&mut self, base: usize, len: usize) {
        if let std::collections::hash_map::Entry::Vacant(e) = self.pinned_hosts.entry(base) {
            let st = (self.cuda.cuMemHostRegister_v2)(base as *mut c_void, len, 0);
            e.insert(if st == CUresult::CUDA_SUCCESS { len } else { 0 });
        }
    }

    /// Drop every page-locked host registration, under the pushed CUDA context.
    ///
    /// Called when the capture's shm segments are recreated at unchanged dimensions: the new
    /// segments often reuse the old base addresses, so a stale registration would alias fresh memory.
    /// Subsequent uploads re-pin lazily. A `0`-length entry marks a registration that failed and so
    /// is not unregistered.
    pub fn release_pinned_hosts(&mut self) {
        if self.pinned_hosts.is_empty() {
            return;
        }
        unsafe {
            let _ = (self.cuda.cuCtxPushCurrent_v2)(self.cuda_context);
            for (base, len) in self.pinned_hosts.drain() {
                if len > 0 {
                    (self.cuda.cuMemHostUnregister)(base as *mut c_void);
                }
            }
            let _ = (self.cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
        }
    }

    /// Tear down one cached dmabuf import under the already-current CUDA context, inner handle
    /// first: the NVENC mapping and registration a direct import holds, then the CUDA graphics
    /// resource, then the EGLImage it was built from. The encode that last read the import has
    /// completed (`submit_frame` waits for the bitstream), so nothing is still in flight on it.
    unsafe fn release_dmabuf_import(&self, cache: CachedDmaBuf) {
        if cache.tex != 0 {
            (self.cuda.cuTexObjectDestroy)(cache.tex);
        }
        if let DmaBufInput::Direct {
            registered, mapped, ..
        } = cache.input
        {
            (self.nvenc_funcs.nvEncUnmapInputResource.unwrap())(self.encoder_session, mapped);
            (self.nvenc_funcs.nvEncUnregisterResource.unwrap())(self.encoder_session, registered);
        }
        (self.cuda.cuGraphicsUnregisterResource)(cache.cuda_resource);
        if let Some(egl) = &self.egl {
            (egl.eglDestroyImageKHR)(self.egl_display, cache.egl_image);
        }
    }

    /// Register the packed input surface with NVENC in the byte order `format` names, when it is
    /// not already: the surface memory is unchanged, only its registration (and mapping) is
    /// replaced, so a session fed first from one source order and then the other keeps one surface.
    /// Runs under the already-current CUDA context; a failed re-registration leaves the surface
    /// unregistered and returns `Err`, so the caller's encode fails visibly instead of encoding
    /// swapped channels.
    unsafe fn set_input_format(&mut self, format: NV_ENC_BUFFER_FORMAT) -> Result<(), String> {
        if self.input_format == format && !self.registered_input_resource.is_null() {
            return Ok(());
        }
        if !self.mapped_input_buffer.is_null() {
            (self.nvenc_funcs.nvEncUnmapInputResource.unwrap())(
                self.encoder_session,
                self.mapped_input_buffer,
            );
            self.mapped_input_buffer = ptr::null_mut();
        }
        if !self.registered_input_resource.is_null() {
            (self.nvenc_funcs.nvEncUnregisterResource.unwrap())(
                self.encoder_session,
                self.registered_input_resource,
            );
            self.registered_input_resource = ptr::null_mut();
        }
        let mut reg_res = NV_ENC_REGISTER_RESOURCE {
            version: sv(NvStruct::RegisterResource),
            resourceType: NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR,
            width: self.width,
            height: self.height,
            resourceToRegister: self.input_device_ptr as *mut c_void,
            pitch: self.input_pitch as u32,
            bufferFormat: format,
            bufferUsage: NV_ENC_BUFFER_USAGE::NV_ENC_INPUT_IMAGE,
            ..Default::default()
        };
        if (self.nvenc_funcs.nvEncRegisterResource.unwrap())(self.encoder_session, &mut reg_res)
            != NVENCSTATUS::NV_ENC_SUCCESS
        {
            return Err(format!("Failed to register input buffer as {format:?}"));
        }
        let mut map_params = NV_ENC_MAP_INPUT_RESOURCE {
            version: sv(NvStruct::MapInputResource),
            registeredResource: reg_res.registeredResource,
            ..Default::default()
        };
        if (self.nvenc_funcs.nvEncMapInputResource.unwrap())(self.encoder_session, &mut map_params)
            != NVENCSTATUS::NV_ENC_SUCCESS
        {
            (self.nvenc_funcs.nvEncUnregisterResource.unwrap())(
                self.encoder_session,
                reg_res.registeredResource,
            );
            return Err(format!("Failed to map input buffer as {format:?}"));
        }
        self.registered_input_resource = reg_res.registeredResource;
        self.mapped_input_buffer = map_params.mappedResource;
        self.input_format = format;
        Ok(())
    }

    /// Hand the live config to `nvEncReconfigureEncoder`, resetting the encoder and forcing an IDR
    /// as asked.
    unsafe fn reconfigure(&mut self, reset: bool, force_idr: bool) -> NVENCSTATUS {
        self.init_params.encodeConfig = &mut self.encode_config;
        let mut params = reconfigure_params(self.init_params, nvenc_cur_ver(), reset, force_idr);
        (self.nvenc_funcs.nvEncReconfigureEncoder.unwrap())(self.encoder_session, &mut params.value)
    }

    /// Reconfigure the live session's ConstQP when the quantizer the session quality index
    /// `crf` selects differs from the current one, returning whether a reconfigure actually
    /// happened.
    ///
    /// A no-op in CBR mode (bitrate-controlled, so QP-based paint-over does not apply), save a
    /// capped constant quantizer bound to its peak, whose floor follows (`peak_of`), and when the
    /// QP is unchanged. When it does apply, the three `constQP` fields are updated and the session is
    /// reconfigured **without** a forced IDR: a lower-QP P-frame refines the static image against the
    /// existing reference chain (paint-over) with no intra-frame bitrate spike, so the GOP continues
    /// seamlessly across the reconfigure.
    unsafe fn reconfigure_if_needed(&mut self, crf: u32) -> bool {
        let binding = self.peak.is_some_and(|p| p.binding);
        if self.encode_config.rcParams.rateControlMode
            == NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR
            && !binding
        {
            return false;
        }
        let target_qp = self.codec.hardware_quantizer(Hardware::Nvenc, crf as i32);
        if self.current_qp != target_qp {
            let rc = &mut self.encode_config.rcParams;
            rc.constQP.qpInterP = target_qp;
            rc.constQP.qpInterB = target_qp;
            rc.constQP.qpIntra = target_qp;
            if binding {
                rc.minQP.qpInterP = target_qp;
                rc.minQP.qpInterB = target_qp;
                rc.minQP.qpIntra = target_qp;
            }
            let status = self.reconfigure(false, false);
            if status == NVENCSTATUS::NV_ENC_SUCCESS {
                self.current_qp = target_qp;
                return true;
            }
            eprintln!(
                "[NVENC] Quantizer reconfigure refused ({status:?}): {}",
                last_error(&self.nvenc_funcs, self.encoder_session)
            );
        }
        false
    }

    /// Whether the constant quantizer runs under a peak (`peak_of`).
    pub fn capped(&self) -> bool {
        self.peak.is_some()
    }

    /// Whether the session is bound to that peak, coding through its constant rate.
    pub fn peak_bound(&self) -> bool {
        self.peak.is_some_and(|p| p.binding)
    }

    /// The quality index the rate control last coded a frame at, where the driver reports its
    /// quantizer; a held frame (`hold_quantizer`) is not the rate control's and leaves it.
    pub fn last_quality(&self) -> Option<u32> {
        self.last_quality
    }

    /// The bytes of the last frame the rate control coded; a held frame leaves it.
    pub fn last_size(&self) -> Option<usize> {
        self.last_bytes
    }

    /// Encode the next frame at the constant quantizer the quality index `crf` selects, whatever
    /// the rate control, and leave the session's own rate control and quantizer as they were for
    /// the frame after: the cleanup of a still screen. A held key frame of a constant-rate session
    /// that comes out past `HELD_KEY_BUDGET_S` of the target is coded again, as a key frame, at the
    /// coarser quantizer `held_key_retry` picks, and so is a held frame of the whole picture past
    /// `HELD_REFRESH_LIMIT_S`, predicted from the frame before the first attempt, which the device
    /// leaves out as it does a frame the client lost (`retract_last`). `band`, the share of the
    /// picture from and to in raster order, confines `crf` to the blocks it covers (`band_size`),
    /// the rest of the frame held at the coarsest quantizer, or for AV1 at the coarsest the delta
    /// map's 128 steps reach above the band: a still region is left as it is at any quantizer, and
    /// a change the frame carries before its damage is known (X11 under Turbo hashes a frame beside
    /// its encode) costs what the rate control's own frame would.
    pub fn hold_quantizer(&mut self, crf: u32, band: Option<(f64, f64)>) {
        self.held_qp = Some(crf);
        self.held_band = band.filter(|_| self.band_size().is_some());
    }

    /// Whether the session refines blocks through a QP delta map under its rate control
    /// (`refine_blocks`): H.264, HEVC, and AV1, until the driver refuses the map.
    pub fn refines(&self) -> bool {
        matches!(self.codec, Codec::H264 | Codec::H265 | Codec::Av1) && !self.refine_refused
    }

    /// Code the blocks `refinement` names in the next frame of the rate control's at its target
    /// quality, through the frame's QP delta map: each map unit (a 16x16 macroblock, a 32x32
    /// coding tree block, a 64x64 superblock) holding a refined block takes the delta from the
    /// quantizer the rate control last coded at to the target, the rest 0. The rate control
    /// is put on the delta map mode the first time, by the reconfigure that takes the
    /// rate-control mode without a reset, and stays there.
    pub fn refine_blocks(&mut self, refinement: &crate::ledger::Refinement) {
        self.refinement = self.refines().then(|| refinement.clone());
    }

    /// Fill `qp_map` for `refinement` (`refine_blocks`), returning whether the session codes
    /// through it, which a refused reconfigure ends for good.
    unsafe fn refine_map(&mut self, refinement: &crate::ledger::Refinement) -> bool {
        if !self.delta_mapped {
            let saved = self.encode_config.rcParams;
            self.encode_config.rcParams.qpMapMode = NV_ENC_QP_MAP_MODE::NV_ENC_QP_MAP_DELTA;
            let status = self.reconfigure(false, false);
            if status != NVENCSTATUS::NV_ENC_SUCCESS {
                self.encode_config.rcParams = saved;
                self.refine_refused = true;
                eprintln!(
                    "[NVENC] Refining blocks through a QP delta map refused ({status:?}): {}",
                    last_error(&self.nvenc_funcs, self.encoder_session)
                );
                return false;
            }
            self.delta_mapped = true;
        }
        let hw = |index: i32| self.codec.hardware_quantizer(Hardware::Nvenc, index) as i32;
        let delta = (hw(refinement.target) - hw(refinement.from)).clamp(i8::MIN as i32, 0) as i8;
        let unit = match self.codec {
            Codec::H264 => 16,
            Codec::H265 => 32,
            _ => 64,
        };
        let (cols, rows) = (self.width.div_ceil(unit), self.height.div_ceil(unit));
        let plan = &refinement.plan;
        self.qp_map.clear();
        self.qp_map.extend((0..rows).flat_map(|row| {
            (0..cols).map(move |col| {
                let (x, y, u) = ((col * unit) as i32, (row * unit) as i32, unit as i32);
                if plan.refines_area(x, y, u, u) {
                    delta
                } else {
                    0
                }
            })
        }));
        true
    }

    /// Take the next frame back where it comes out past `bytes` and code it again within them: a
    /// held band in a shorter one, to `REFIT_SHARE` of them at the bytes the band came to, and a
    /// frame of the rate control's at the coarsest quantizer, which on a still screen refines
    /// nothing. The frame taken back is left out of the predictions as a frame a client lost is
    /// (`retract_last`); a key frame stands as coded.
    pub fn cap(&mut self, bytes: usize) {
        self.cap = Some(bytes);
    }

    /// Where the last frame was taken back and coded again (`cap`): how far its held band then
    /// reached, or 0 for a frame of the rate control's.
    pub fn refit(&self) -> Option<f64> {
        self.refit
    }

    /// Whether a frame left a held band out of every prediction since this was last asked (an
    /// anchor predicting from the newest frame every client was sent, past a band coded since),
    /// where the frame held none to take it back into: the cleanup has to run again.
    pub fn take_lost_cleanup(&mut self) -> bool {
        std::mem::take(&mut self.lost_cleanup)
    }

    /// The frames from timestamp `pts` on have just been left out of every prediction: a band one
    /// of them held is coded again in the band this frame holds, which then reaches back to it,
    /// and is lost to a frame that holds none (`take_lost_cleanup`).
    fn forget_bands(&mut self, pts: u64) {
        let (band, lost) = reach_back(
            &mut self.band_log,
            pts,
            self.held_band,
            self.held_qp.is_some(),
        );
        self.held_band = band;
        self.lost_cleanup |= lost;
    }

    /// Keep the deblocking filter off the rows the bands before refined, on a frame holding a band
    /// whose first block is `band` (`None` on any other frame): at the edges a band shares with
    /// blocks held at the coarsest quantizer, the filter works at their mean quantizer and softens
    /// those rows (at 60 fps 0.5 Mbit/s a 1080p row from 44.5 dB to 33.9). H.264 turns the filter
    /// off for the frame, which its slice header carries. NVENC's HEVC codes the same bytes
    /// whatever its switch says on a reconfigure, so its band frame starts a slice at the band
    /// instead, across whose boundary the session never filters (`configure_codec`), where that
    /// takes at most `BAND_SLICES_MAX` even slices; a band starting before them keeps the frame's
    /// own. AV1 has neither.
    fn set_band_layout(&mut self, band: Option<usize>) {
        match self.codec {
            Codec::H264 => {
                self.encode_config
                    .encodeCodecConfig
                    .h264Config
                    .disableDeblockingFilterIDC = u32::from(band.is_some());
            }
            Codec::H265 => {
                let blocks = (self.width.div_ceil(32) * self.height.div_ceil(32)) as usize;
                let c = unsafe { &mut self.encode_config.encodeCodecConfig.hevcConfig };
                (c.sliceMode, c.sliceModeData) = match band {
                    Some(first) if first > 0 && blocks.div_ceil(first) <= BAND_SLICES_MAX => {
                        (0, first as u32)
                    }
                    _ => self.slice_layout,
                };
            }
            _ => {}
        }
    }

    /// The bytes of the last held frame (0 before one), where the session holds a band: H.264,
    /// HEVC and AV1, whose QP delta map is one entry a macroblock, a 32x32 coding tree block, or a
    /// 64x64 superblock.
    pub fn band_size(&self) -> Option<usize> {
        matches!(self.codec, Codec::H264 | Codec::H265 | Codec::Av1).then_some(self.held_bytes)
    }

    /// Put the session on constant quantizer `q` for the picture about to be submitted, with the
    /// picture's QP delta map on top where it holds a band starting at block `band`
    /// (`fill_band_map`), answering the rate control to restore once it is encoded; `None` where
    /// the session is already there or the driver refused, which leaves the picture to the rate
    /// control.
    ///
    /// The rate-control mode is one of the parameters `nvEncReconfigureEncoder` takes without a
    /// reset, so a constant-rate session keeps its HRD state across the held picture instead of
    /// starting over after it.
    unsafe fn hold_rate(&mut self, q: u32, band: Option<usize>) -> Option<NV_ENC_RC_PARAMS> {
        let mapped = band.is_some();
        let saved = self.encode_config.rcParams;
        let constant = saved.rateControlMode == NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CONSTQP;
        if constant && !mapped && saved.constQP.qpIntra == q && saved.constQP.qpInterP == q {
            return None;
        }
        self.set_band_layout(band);
        let rc = &mut self.encode_config.rcParams;
        if mapped {
            rc.qpMapMode = NV_ENC_QP_MAP_MODE::NV_ENC_QP_MAP_DELTA;
        }
        rc.rateControlMode = NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CONSTQP;
        rc.constQP.qpInterP = q;
        rc.constQP.qpInterB = q;
        rc.constQP.qpIntra = q;
        let status = self.reconfigure(false, false);
        if status != NVENCSTATUS::NV_ENC_SUCCESS {
            self.encode_config.rcParams = saved;
            self.set_band_layout(None);
            if !std::mem::replace(&mut self.hold_refused, true) {
                eprintln!(
                    "[NVENC] Holding a frame at quantizer {q} refused ({status:?}): {}",
                    last_error(&self.nvenc_funcs, self.encoder_session)
                );
            }
            return None;
        }
        Some(saved)
    }

    /// Apply a runtime rate-control / frame-rate change to the live session, and report whether
    /// the session carries it afterwards.
    ///
    /// In CBR mode the target bitrate, max bitrate, VBV, and its initial delay are updated (the VBV
    /// is ignored outside CBR); the target fps is updated in either mode. The session is
    /// reconfigured only when one of these actually changed — no RC reset, and a forced IDR only
    /// where a target past the declared level's bitrate ceiling, or a frame rate past its
    /// macroblock rate, raises the level, which the decoder learns from the sequence header a key
    /// frame carries; a level a lower target or frame rate no longer needs stays, since a level
    /// only ever has to be high enough and the driver refuses one below the decoded picture
    /// buffer the session declared — so calling it every frame is cheap. A reconfigure the driver
    /// refuses leaves the session encoding at its previous rate, logged with the driver's reason.
    pub fn reconfigure_rate(&mut self, settings: &CaptureSettings) -> bool {
        if let Some(peak) = self.peak.as_mut() {
            let bps = cbr_bps(settings);
            peak.follow(bps, cbr_vbv(settings, bps), settings.target_fps);
            if peak.binding {
                let rc = &mut self.encode_config.rcParams;
                let at = rc.maxBitRate;
                rc.maxBitRate = peak.bps;
                let past = self.level_for_peak() > self.declared_level();
                self.encode_config.rcParams.maxBitRate = at;
                if past {
                    unsafe { self.switch_peak(false) };
                }
            }
        }
        unsafe {
            let mut changed = false;
            let mut level_raised = false;
            if self.encode_config.rcParams.rateControlMode
                == NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR
            {
                let bps = cbr_bps(settings);
                let vbv = cbr_vbv(settings, bps);
                let rc = &mut self.encode_config.rcParams;
                if rc.averageBitRate != bps || rc.maxBitRate != bps || rc.vbvBufferSize != vbv {
                    log_held_rate(settings, bps);
                    set_cbr_rate(rc, bps, vbv);
                    changed = true;
                    let fps = self
                        .init_params
                        .frameRateNum
                        .div_ceil(self.init_params.frameRateDen.max(1));
                    let (w, h) = (self.init_params.encodeWidth, self.init_params.encodeHeight);
                    if self.level_for(w, h, fps) > self.declared_level() {
                        self.set_level(w, h, fps);
                        level_raised = true;
                    }
                }
            }
            let rate = FrameRate::of(settings.target_fps);
            if (self.init_params.frameRateNum, self.init_params.frameRateDen)
                != (rate.num, rate.den)
            {
                self.init_params.frameRateNum = rate.num;
                self.init_params.frameRateDen = rate.den;
                let (w, h, fps) = (
                    self.init_params.encodeWidth,
                    self.init_params.encodeHeight,
                    rate.ceil(),
                );
                if self.level_for(w, h, fps) > self.declared_level() {
                    self.set_level(w, h, fps);
                    level_raised = true;
                }
                changed = true;
            }
            if !changed {
                return true;
            }
            let mut status = self.reconfigure(false, level_raised);
            if status != NVENCSTATUS::NV_ENC_SUCCESS
                && self.codec == Codec::Av1
                && !NVENC_AV1_WEIGHTED.swap(true, Ordering::Relaxed)
            {
                let (w, h) = (self.init_params.encodeWidth, self.init_params.encodeHeight);
                let fps = self
                    .init_params
                    .frameRateNum
                    .div_ceil(self.init_params.frameRateDen.max(1));
                let refused = self.declared_level();
                if self.level_for(w, h, fps) > refused {
                    self.set_level(w, h, fps);
                    level_raised = true;
                    status = self.reconfigure(false, true);
                }
                if status == NVENCSTATUS::NV_ENC_SUCCESS {
                    eprintln!(
                        "[NVENC] The driver refused AV1 level {refused} at {} kbps; declaring {}",
                        self.encode_config.rcParams.maxBitRate / 1000,
                        self.declared_level()
                    );
                } else {
                    NVENC_AV1_WEIGHTED.store(false, Ordering::Relaxed);
                }
            }
            if status != NVENCSTATUS::NV_ENC_SUCCESS {
                eprintln!(
                    "[NVENC] Rate reconfigure refused ({status:?}): {}",
                    last_error(&self.nvenc_funcs, self.encoder_session)
                );
                return false;
            }
            if level_raised && let Some(references) = &mut self.references {
                references.reset();
            }
            true
        }
    }

    /// Encode one mapped input picture and return its bitstream bytes behind the wire header.
    ///
    /// The shared tail of all three encode paths:
    ///
    /// 1. **Pick an output buffer** from the ring (`current_buffer_idx` advances modulo the ring
    ///    length) and submit the picture with `nvEncEncodePicture`; `force_idr` sets the force-IDR
    ///    pic flag.
    /// 2. **Lock the bitstream** (`nvEncLockBitstream`, blocking) to read the encoded bytes. The
    ///    lock has to block: on Linux a `doNotWait` lock answers an unfinished encode with
    ///    `NV_ENC_SUCCESS` and an empty bitstream, not `NV_ENC_ERR_LOCK_BUSY`, so polling it would
    ///    hand out empty frames.
    /// 3. **Frame the output**: unless `omit_stripe_headers` is set, prepend the wire header with
    ///    the frame kind derived from the *actual* encoded `pictureType` (IDR = key, I = intra,
    ///    else delta) rather than the `force_idr` request.
    /// 4. **Emit**: append the encoded bytes, unlock the bitstream, and return the framed buffer.
    unsafe fn submit_frame(
        &mut self,
        mapped_buffer: NV_ENC_INPUT_PTR,
        buffer_format: NV_ENC_BUFFER_FORMAT,
        frame_number: u64,
        force_idr: bool,
    ) -> Result<Vec<u8>, String> {
        let output_bitstream = self.bitstream_buffers[self.current_buffer_idx];
        self.current_buffer_idx = (self.current_buffer_idx + 1) % self.bitstream_buffers.len();

        let asked_idr = force_idr;
        let mut force_idr =
            force_idr || self.references.as_ref().is_some_and(|r| !r.has_reference());
        let mut forgot = None;
        if !force_idr
            && let Some(references) = &mut self.references
            && references.plan_anchor(false).is_some()
        {
            match references.settle() {
                Some(Invalidation::Forget(pts)) => {
                    force_idr = (self.nvenc_funcs.nvEncInvalidateRefFrames.unwrap())(
                        self.encoder_session,
                        pts,
                    ) != NVENCSTATUS::NV_ENC_SUCCESS;
                    forgot = Some(pts);
                }
                Some(Invalidation::KeyFrame) => force_idr = true,
                _ => {}
            }
        }
        if !force_idr && let Some(references) = &mut self.references {
            match references.forget_stale() {
                Some(Invalidation::Forget(pts)) => {
                    force_idr = (self.nvenc_funcs.nvEncInvalidateRefFrames.unwrap())(
                        self.encoder_session,
                        pts,
                    ) != NVENCSTATUS::NV_ENC_SUCCESS;
                    forgot = Some(forgot.map_or(pts, |f: u64| f.min(pts)));
                }
                Some(Invalidation::KeyFrame) => force_idr = true,
                _ => {}
            }
        }
        if force_idr {
            // A key frame of the device's own codes the picture at the stream's quality, or with
            // a band at the coarsest outside it: the bands before are gone.
            if !asked_idr
                && !self.band_log.is_empty()
                && (self.held_band.is_some() || self.held_qp.is_none())
            {
                self.lost_cleanup = true;
            }
            self.band_log.clear();
        } else if let Some(pts) = forgot {
            self.forget_bands(pts);
        }
        let anchor = self
            .references
            .as_ref()
            .and_then(|r| r.plan_anchor(force_idr));
        let named = self
            .references
            .as_ref()
            .filter(|_| !force_idr)
            .and_then(ReferenceWindow::predicting_anchor);
        let mut pic_params = NV_ENC_PIC_PARAMS {
            version: sv(NvStruct::PicParams),
            inputWidth: self.width,
            inputHeight: self.height,
            inputBuffer: mapped_buffer,
            outputBitstream: output_bitstream,
            bufferFmt: buffer_format,
            pictureStruct: NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME,
            encodePicFlags: if force_idr {
                NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_FORCEIDR as u32
            } else {
                0
            },
            inputTimeStamp: self
                .references
                .as_ref()
                .map_or(0, ReferenceWindow::next_pts),
            ..Default::default()
        };

        macro_rules! long_term {
            ($p:expr) => {{
                let p = $p;
                if let Some(slot) = named {
                    p.set_ltrUseFrames(1);
                    p.ltrUseFrameBitmap = 1 << slot;
                }
                if let Some(slot) = anchor {
                    p.set_ltrMarkFrame(1);
                    p.ltrMarkFrameIdx = slot as u32;
                }
            }};
        }
        match self.codec {
            Codec::Av1 => long_term!(&mut pic_params.codecPicParams.av1PicParams),
            Codec::H265 => long_term!(&mut pic_params.codecPicParams.hevcPicParams),
            _ => long_term!(&mut pic_params.codecPicParams.h264PicParams),
        }
        let held_qp = self.held_qp.take();
        let band = self.held_band.take();
        let held = held_qp.and_then(|crf| {
            let q = self.codec.hardware_quantizer(Hardware::Nvenc, crf as i32);
            let rest = self
                .codec
                .quantizer_max()
                .min(q + i8::MIN.unsigned_abs() as u32);
            match band {
                Some(share) if rest > q => {
                    let first = fill_band_map(
                        &mut self.qp_map,
                        self.codec,
                        self.width,
                        self.height,
                        share,
                        q as i32 - rest as i32,
                    );
                    let held = self.hold_rate(rest, Some(first));
                    if held.is_some() {
                        point_at_qp_map(&mut pic_params, &mut self.qp_map, nvenc_cur_ver());
                    }
                    held
                }
                _ => self.hold_rate(q, None),
            }
        });
        let refinement = self
            .refinement
            .take()
            .filter(|_| held_qp.is_none() && !force_idr);
        if let Some(refinement) = refinement
            && self.refine_map(&refinement)
        {
            point_at_qp_map(&mut pic_params, &mut self.qp_map, nvenc_cur_ver());
        }
        let mut result = self.encode_picture(
            &mut pic_params,
            output_bitstream,
            frame_number,
            held_qp.is_some(),
            anchor,
        );
        let retry = match (held_qp, held, &result) {
            (Some(crf), Some(rc), Ok(coded))
                if rc.rateControlMode == NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR
                    && (force_idr
                        || band.is_none()
                            && coded.len() as f64
                                > rc.averageBitRate as f64 / 8.0 * super::HELD_REFRESH_LIMIT_S) =>
            {
                let cap = (rc.averageBitRate as f64 / 8.0 * super::HELD_KEY_BUDGET_S) as usize;
                super::held_key_retry(crf, coded.len(), cap)
            }
            _ => None,
        };
        if let Some(coarser) = retry
            && self
                .hold_rate(
                    self.codec
                        .hardware_quantizer(Hardware::Nvenc, coarser as i32),
                    None,
                )
                .is_some()
            && (force_idr || self.retract_last())
        {
            pic_params.inputTimeStamp = self
                .references
                .as_ref()
                .map_or(0, ReferenceWindow::next_pts);
            result = self.encode_picture(
                &mut pic_params,
                output_bitstream,
                frame_number,
                true,
                anchor,
            );
        }
        self.refit = None;
        let cap = self.cap.take();
        let mut held = held;
        if let (Some(cap), Ok(coded), None, false) = (cap, &result, retry, force_idr)
            && coded.len() > cap
        {
            let refit = match (held_qp, band, held) {
                (Some(crf), Some((from, to)), Some(_)) => {
                    let q = self.codec.hardware_quantizer(Hardware::Nvenc, crf as i32);
                    let rest = self
                        .codec
                        .quantizer_max()
                        .min(q + i8::MIN.unsigned_abs() as u32);
                    let end = from + (to - from) * REFIT_SHARE * cap as f64 / coded.len() as f64;
                    fill_band_map(
                        &mut self.qp_map,
                        self.codec,
                        self.width,
                        self.height,
                        (from, end),
                        q as i32 - rest as i32,
                    );
                    point_at_qp_map(&mut pic_params, &mut self.qp_map, nvenc_cur_ver());
                    Some(end)
                }
                (None, _, None) => {
                    held = self.hold_rate(self.codec.quantizer_max(), None);
                    held.map(|_| 0.0)
                }
                _ => None,
            };
            if let Some(end) = refit
                && self.retract_last()
            {
                pic_params.inputTimeStamp = self
                    .references
                    .as_ref()
                    .map_or(0, ReferenceWindow::next_pts);
                result = self.encode_picture(
                    &mut pic_params,
                    output_bitstream,
                    frame_number,
                    true,
                    anchor,
                );
                self.refit = Some(end);
            }
        }
        if let (Some(_), Some((from, to)), Ok(_)) = (held, band, &result) {
            let to = self.refit.unwrap_or(to);
            self.band_log
                .push_back((pic_params.inputTimeStamp, from, to));
            if self.band_log.len() > BAND_LOG_FRAMES {
                self.band_log.pop_front();
            }
        }
        if let Some(rc) = held {
            self.encode_config.rcParams = rc;
            self.set_band_layout(None);
            let status = self.reconfigure(false, false);
            if status != NVENCSTATUS::NV_ENC_SUCCESS {
                eprintln!(
                    "[NVENC] Restoring the rate control after a held frame refused ({status:?}): {}",
                    last_error(&self.nvenc_funcs, self.encoder_session)
                );
            }
        }
        if let Ok(coded) = &result {
            self.follow_peak(coded.len(), held_qp.is_none());
        }
        result
    }

    /// Fold the frame just coded, `bytes` long, into a capped constant quantizer's peak
    /// (`peak_of`), and bind the session to the peak or release it where the frame says to; `own`
    /// is false for a held frame. A frame refining blocks through the QP map counts as the rate
    /// control's: bound to a peak its content overflows, NVENC's constant rate holds such a
    /// frame's mean quantizer over the floor (none of 140 refining every block 25 steps finer
    /// came out at it, on an L4), and a session refining on every bound frame would otherwise
    /// never count one toward its release.
    unsafe fn follow_peak(&mut self, bytes: usize, own: bool) {
        let floor = self.current_qp;
        let qp = Some(self.last_avg_qp).filter(|q| own && *q > 0);
        let Some(peak) = self.peak.as_mut() else {
            return;
        };
        if let Some(bind) = peak.note(bytes as f64 * 8.0, qp.map(|q| q <= floor)) {
            self.switch_peak(bind);
        }
    }

    /// Bind a capped constant quantizer to its peak, or release it to its constant quantizer
    /// (`peak_of`); a switch the driver refuses leaves the session as it was. A peak past the
    /// level the session declared releases it rather than raise the level, a key frame.
    unsafe fn switch_peak(&mut self, bind: bool) {
        let Some(peak) = self.peak else {
            return;
        };
        let floor = self.current_qp;
        let (bps, vbv) = (peak.bps, peak.vbv);
        let saved = self.encode_config.rcParams;
        let rc = &mut self.encode_config.rcParams;
        if bind {
            rc.rateControlMode = NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR;
            set_cbr_rate(rc, bps, vbv);
            rc.set_enableMinQP(1);
            rc.minQP.qpInterP = floor;
            rc.minQP.qpInterB = floor;
            rc.minQP.qpIntra = floor;
        } else {
            let open = self.open_rc;
            rc.rateControlMode = open.rateControlMode;
            rc.averageBitRate = open.averageBitRate;
            rc.maxBitRate = open.maxBitRate;
            rc.vbvBufferSize = open.vbvBufferSize;
            rc.vbvInitialDelay = open.vbvInitialDelay;
            // The driver keeps coding at the floor it last took once `enableMinQP` is cleared
            // (an L4), so a release lowers the floor to the lowest quantizer instead.
            rc.set_enableMinQP(1);
            rc.minQP.qpInterP = 0;
            rc.minQP.qpInterB = 0;
            rc.minQP.qpIntra = 0;
        }
        if bind && self.level_for_peak() > self.declared_level() {
            self.encode_config.rcParams = saved;
            if let Some(peak) = self.peak.as_mut() {
                peak.binding = false;
            }
            return;
        }
        let status = self.reconfigure(false, false);
        if status != NVENCSTATUS::NV_ENC_SUCCESS {
            self.encode_config.rcParams = saved;
            if let Some(peak) = self.peak.as_mut() {
                peak.binding = !bind;
            }
            if !std::mem::replace(&mut self.hold_refused, true) {
                eprintln!(
                    "[NVENC] {} the peak of a capped constant quantizer refused ({status:?}): {}",
                    if bind { "Binding to" } else { "Releasing" },
                    last_error(&self.nvenc_funcs, self.encoder_session)
                );
            }
            return;
        }
        if let Some(peak) = self.peak.as_mut() {
            peak.binding = bind;
        }
    }

    /// The level the live config takes at the session's frame rate and size.
    fn level_for_peak(&self) -> u32 {
        let (w, h) = (self.init_params.encodeWidth, self.init_params.encodeHeight);
        let fps = self
            .init_params
            .frameRateNum
            .div_ceil(self.init_params.frameRateDen.max(1));
        self.level_for(w, h, fps)
    }

    /// Submit one picture and read its bitstream back behind the wire header: steps 1 to 4 of
    /// `submit_frame` past the choice of output buffer. A `held` picture leaves `last_quality` to
    /// the rate control's own frames.
    unsafe fn encode_picture(
        &mut self,
        pic_params: &mut NV_ENC_PIC_PARAMS,
        output_bitstream: NV_ENC_OUTPUT_PTR,
        frame_number: u64,
        held: bool,
        anchor: Option<u8>,
    ) -> Result<Vec<u8>, String> {
        let encode_fn = self.nvenc_funcs.nvEncEncodePicture.unwrap();
        let res = encode_fn(self.encoder_session, pic_params);
        if res != NVENCSTATUS::NV_ENC_SUCCESS {
            return Err(format!("Encode Picture failed: {:?}", res));
        }
        // A host upload's band hash asked for (`hash_next_upload`), queued once its encode is, so
        // it runs on the CUDA cores beside the encode engine and holds nothing up.
        if let Some(rows) = self.upload_hash.take() {
            let (src, pitch, width, height) = (
                self.input_device_ptr,
                self.input_pitch,
                self.width,
                self.height,
            );
            let cuda = &self.cuda;
            self.upload_words = self
                .band_hash
                .get_or_insert_with(|| BandHash::new(cuda))
                .as_mut()
                .and_then(|h| h.launch(cuda, src, pitch, width, height, rows));
        }

        let mut negotiated = Negotiated::new(NV_ENC_LOCK_BITSTREAM {
            version: sv(NvStruct::LockBitstream),
            outputBitstream: output_bitstream,
            ..Default::default()
        });
        let lock_params = &mut negotiated.value;
        lock_params.set_doNotWait(0);
        let lock_fn = self.nvenc_funcs.nvEncLockBitstream.unwrap();
        let status = lock_fn(self.encoder_session, lock_params);
        if status != NVENCSTATUS::NV_ENC_SUCCESS {
            return Err(format!("Lock Bitstream failed: {status:?}"));
        }

        let data_ptr = lock_params.bitstreamBufferPtr as *const u8;
        let data_size = lock_params.bitstreamSizeInBytes as usize;
        let header_sz = if self.omit_stripe_headers {
            0
        } else {
            VIDEO_HEADER_LEN
        };
        let mut output = Vec::with_capacity(header_sz + data_size);
        if !held {
            self.last_avg_qp = lock_params.frameAvgQP;
            self.last_quality = match lock_params.frameAvgQP {
                0 => None,
                q => Some(self.codec.hardware_quality_index(Hardware::Nvenc, q)),
            };
            self.last_bytes = Some(data_size);
        } else {
            self.held_bytes = data_size;
        }
        let frame_type = match lock_params.pictureType {
            NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR => FRAME_KEY,
            NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_I => FRAME_INTRA,
            _ => FRAME_DELTA,
        };
        let mut wire_type = frame_type;
        if let Some(references) = &mut self.references {
            let shared = references.acknowledging() && references.predicts_from_shared();
            self.last_reference =
                references.record_marked(frame_number as u16, frame_type != FRAME_DELTA, anchor);
            if anchor.is_some() && frame_type == FRAME_DELTA && shared {
                wire_type |= FRAME_ANCHOR;
            }
        }

        if !self.omit_stripe_headers {
            push_video_header(
                &mut output,
                self.codec,
                wire_type,
                frame_number as u16,
                0,
                self.width as u16,
                self.height as u16,
                self.last_reference,
            );
        }

        if data_size > 0 && !data_ptr.is_null() {
            let slice = std::slice::from_raw_parts(data_ptr, data_size);
            if self.codec == Codec::H264 && self.references.is_some() {
                let mut widened = true;
                for nal in crate::encoders::codec::annexb_nals(slice) {
                    widened &= self.frame_num.push_unit(&[0, 0, 0, 1], nal, &mut output);
                }
                if !widened && let Some(references) = &mut self.references {
                    // Its slices no longer match the set they went out under: the next frame
                    // is a key frame.
                    references.reset();
                }
            } else {
                output.extend_from_slice(slice);
            }
        }
        if frame_type == FRAME_KEY
            && self.codec == Codec::H264
            && let Some(references) = &mut self.references
        {
            references.set_frame_num_range(h264_frame_num_range(&output[header_sz..]).unwrap_or(0));
        }

        (self.nvenc_funcs.nvEncUnlockBitstream.unwrap())(self.encoder_session, output_bitstream);
        Ok(output)
    }

    /// Encode a dmabuf frame zero-copy, by importing it through EGL into CUDA and, where the
    /// driver allows, handing the mapped plane to NVENC as its input.
    ///
    /// Applies any pending ConstQP change, then works under the pushed CUDA context:
    ///
    /// 1. **Import once, cache by fd with an identity check**: the cache is keyed by the dmabuf fd
    ///    but each entry stores the buffer's `DmaBufIdentity`; an entry whose identity no longer
    ///    matches (a recycled fd) is released first. On a miss, build an `EGLImageKHR` from the
    ///    dmabuf's fd / offset / pitch / modifier, register it as a CUDA graphics resource, map it to
    ///    a `CUeglFrame`, and settle how it feeds the encoder: a first plane that `direct_plane`
    ///    accepts (and `direct_dmabuf` on) is registered with NVENC in place — a pitch-linear plane
    ///    as a CUDA device pointer at its own pitch, a four-channel 8-bit CUDA array as a CUDA
    ///    array — in the byte order the dmabuf fourcc names, and mapped once (`DmaBufInput::Direct`);
    ///    any other plane, or a registration the driver refuses, takes `DmaBufInput::Copy`. The
    ///    result is memoized so a recurring capture buffer pays the import cost only once. Each
    ///    failure destroys what it created and pops the context.
    /// 2. **Feed the encoder**: a direct import is submitted as it is — no copy at all. A copy
    ///    import is copied with `cuMemcpy2DAsync` on the default stream — the array plane or the
    ///    pitch-linear plane, per `frame_type` — into the packed input surface, re-registered in the
    ///    dmabuf's byte order when it differs; NVENC processes its input on that same stream, so the
    ///    copy is ordered before the encode without a host wait.
    /// 3. **Submit** via `submit_frame`, then pop the context.
    ///
    /// The dmabuf fd is read out before the context is pushed so an early `?` return cannot leave the
    /// CUDA context stack imbalanced.
    pub fn encode(
        &mut self,
        dmabuf: &Dmabuf,
        frame_number: u64,
        crf: u32,
        force_idr: bool,
    ) -> Result<Vec<u8>, String> {
        self.encode_frame(dmabuf, frame_number, crf, force_idr, false)
    }

    /// `encode`, for a buffer the X server is still blitting into: the frame's work is queued
    /// behind a wait on the semaphore the server signals once the blit has landed
    /// (`blit_semaphore_fd`), so it runs on the GPU right behind the blit instead of after the
    /// CPU has learnt of it. NVENC reads its input on the default stream, as the copy and the
    /// convert do, so one wait there orders all of it.
    pub fn encode_after_blit(
        &mut self,
        dmabuf: &Dmabuf,
        frame_number: u64,
        crf: u32,
        force_idr: bool,
    ) -> Result<Vec<u8>, String> {
        self.encode_frame(dmabuf, frame_number, crf, force_idr, true)
    }

    /// A new semaphore for the X server to signal after a blit and `encode_after_blit` to wait
    /// on: made on this session's GPU, imported into its context in place of any earlier one,
    /// and returned as the fd the server imports.
    pub fn blit_semaphore_fd(&mut self) -> Result<std::os::fd::OwnedFd, String> {
        use std::os::fd::IntoRawFd;
        unsafe {
            let (Some(import), Some(_), Some(destroy)) = (
                self.cuda.cuImportExternalSemaphore,
                self.cuda.cuWaitExternalSemaphoresAsync,
                self.cuda.cuDestroyExternalSemaphore,
            ) else {
                return Err("the CUDA driver has no external semaphores".into());
            };
            let mut uuid = CUuuid { bytes: [0; 16] };
            if (self.cuda.cuDeviceGetUuid)(&mut uuid, self.cuda_device) != CUresult::CUDA_SUCCESS {
                return Err("the CUDA device has no UUID to find its Vulkan device by".into());
            }
            let (cuda_fd, server_fd) =
                super::blit_semaphore::opaque_fd_pair(uuid.bytes.map(|b| b.to_ne_bytes()[0]))?;
            let _ = (self.cuda.cuCtxPushCurrent_v2)(self.cuda_context);
            if !self.blit_semaphore.is_null() {
                destroy(self.blit_semaphore);
                self.blit_semaphore = ptr::null_mut();
            }
            let mut desc = CUDA_EXTERNAL_SEMAPHORE_HANDLE_DESC {
                type_: CUexternalSemaphoreHandleType::CU_EXTERNAL_SEMAPHORE_HANDLE_TYPE_OPAQUE_FD,
                ..Default::default()
            };
            desc.handle.fd = cuda_fd.as_raw_fd();
            let mut semaphore: CUexternalSemaphore = ptr::null_mut();
            let r = import(&mut semaphore, &desc);
            (self.cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
            if r != CUresult::CUDA_SUCCESS {
                return Err(format!(
                    "CUDA refused the semaphore ({})",
                    Self::get_error_string(&self.cuda, r)
                ));
            }
            // A successful import owns its fd.
            let _ = cuda_fd.into_raw_fd();
            self.blit_semaphore = semaphore;
            Ok(server_fd)
        }
    }

    fn encode_frame(
        &mut self,
        dmabuf: &Dmabuf,
        frame_number: u64,
        crf: u32,
        force_idr: bool,
        after_blit: bool,
    ) -> Result<Vec<u8>, String> {
        unsafe {
            self.reconfigure_if_needed(crf);
            let fd = dmabuf.handles().next().ok_or("No handles")?.as_raw_fd();
            let fmt = dmabuf.format();
            let modifier: u64 = fmt.modifier.into();
            let identity = DmaBufIdentity::probe(fd, modifier, self.width, self.height);
            let _ = (self.cuda.cuCtxPushCurrent_v2)(self.cuda_context);

            // First, before anything can fail: each signal the server was asked for has exactly
            // one wait, or the binary semaphore would be signalled again while still signalled.
            if after_blit {
                let wait = self.cuda.cuWaitExternalSemaphoresAsync;
                let queued = match wait {
                    Some(wait) if !self.blit_semaphore.is_null() => {
                        let params = CUDA_EXTERNAL_SEMAPHORE_WAIT_PARAMS::default();
                        wait(&self.blit_semaphore, &params, 1, ptr::null_mut())
                    }
                    _ => CUresult::CUDA_ERROR_NOT_INITIALIZED,
                };
                if queued != CUresult::CUDA_SUCCESS {
                    (self.cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
                    return Err(format!(
                        "the wait for the server's blit could not be queued ({})",
                        Self::get_error_string(&self.cuda, queued)
                    ));
                }
            }

            // A raw fd number is not an identity: the host recycles fd numbers across slot
            // renegotiations, so an entry whose stored identity no longer matches is torn down and
            // re-imported rather than returning a stale EGLImage for a buffer the fd no longer names.
            if self
                .dmabuf_cache
                .get(&fd)
                .is_some_and(|c| c.identity != identity)
                && let Some(stale) = self.dmabuf_cache.remove(&fd)
            {
                self.release_dmabuf_import(stale);
            }

            if !self.dmabuf_cache.contains_key(&fd) {
                let Some(egl) = self.egl.clone() else {
                    (self.cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
                    return Err("a session opened without an EGL display imports no dmabuf".into());
                };
                let stride = dmabuf.strides().next().unwrap_or(0) as i32;
                let offset = dmabuf.offsets().next().unwrap_or(0) as i32;

                let attribs = [
                    EGL_WIDTH,
                    self.width as i32,
                    EGL_HEIGHT,
                    self.height as i32,
                    EGL_LINUX_DRM_FOURCC_EXT,
                    egl_import_fourcc(fmt.code) as i32,
                    EGL_DMA_BUF_PLANE0_FD_EXT,
                    fd,
                    EGL_DMA_BUF_PLANE0_OFFSET_EXT,
                    offset,
                    EGL_DMA_BUF_PLANE0_PITCH_EXT,
                    stride,
                    EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,
                    (modifier & 0xFFFFFFFF) as i32,
                    EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,
                    (modifier >> 32) as i32,
                    EGL_NONE,
                ];

                let egl_image = (egl.eglCreateImageKHR)(
                    self.egl_display,
                    ptr::null_mut(),
                    EGL_LINUX_DMA_BUF_EXT,
                    ptr::null_mut(),
                    attribs.as_ptr(),
                );
                if egl_image == EGL_NO_IMAGE_KHR {
                    (self.cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
                    return Err("Failed to create EGLImage".into());
                }

                let mut cuda_resource: CUgraphicsResource = ptr::null_mut();
                if (self.cuda.cuGraphicsEGLRegisterImage)(&mut cuda_resource, egl_image, 1)
                    != CUresult::CUDA_SUCCESS
                {
                    (egl.eglDestroyImageKHR)(self.egl_display, egl_image);
                    (self.cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
                    return Err("Failed to register EGLImage".into());
                }

                let mut egl_frame: CUeglFrame = std::mem::zeroed();
                if (self.cuda.cuGraphicsResourceGetMappedEglFrame)(
                    &mut egl_frame,
                    cuda_resource,
                    0,
                    0,
                ) != CUresult::CUDA_SUCCESS
                {
                    (self.cuda.cuGraphicsUnregisterResource)(cuda_resource);
                    (egl.eglDestroyImageKHR)(self.egl_display, egl_image);
                    (self.cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
                    return Err("Failed to map EGL frame".into());
                }

                let input = match (
                    self.direct_dmabuf,
                    direct_plane(&egl_frame, self.width, self.height),
                    fourcc_nvenc_format(fmt.code),
                ) {
                    (true, Some(plane), Some(format)) => {
                        self.register_direct_input(&egl_frame, plane, format)
                    }
                    _ => DmaBufInput::Copy,
                };
                crate::log::debug!(
                    "[NVENC] dmabuf imported as a {} frame ({} planes, {}x{}, pitch {}, {} channels of element format {}): {}.",
                    match egl_frame.frame_type {
                        CU_EGL_FRAME_TYPE_PITCH => "pitch-linear",
                        CU_EGL_FRAME_TYPE_ARRAY => "CUDA-array",
                        _ => "unknown-kind",
                    },
                    egl_frame.plane_count,
                    egl_frame.width,
                    egl_frame.height,
                    egl_frame.pitch,
                    egl_frame.num_channels,
                    egl_frame.cu_format,
                    match input {
                        DmaBufInput::Direct { .. } => "encoding in place",
                        DmaBufInput::Copy => "copying per frame",
                    }
                );

                // An array-typed import is read through a texture, so the convert never needs
                // it copied into linear memory.
                let tex = if self.csc.is_some() && egl_frame.frame_type == CU_EGL_FRAME_TYPE_ARRAY {
                    ChromaConvert::texture_for(&self.cuda, egl_frame.frame.p_array[0])
                } else {
                    0
                };
                self.dmabuf_cache.insert(
                    fd,
                    CachedDmaBuf {
                        identity,
                        egl_image,
                        cuda_resource,
                        egl_frame,
                        input,
                        tex,
                    },
                );
            }

            let (egl_frame, input, tex) = {
                let cached = self.dmabuf_cache.get(&fd).unwrap();
                (cached.egl_frame, cached.input, cached.tex)
            };
            let (mapped, format) = match input {
                DmaBufInput::Direct { mapped, format, .. } => (mapped, format),
                DmaBufInput::Copy => {
                    let mut copy_params = CUDA_MEMCPY2D {
                        srcMemoryType: CUmemorytype::CU_MEMORYTYPE_DEVICE,
                        srcHost: ptr::null(),
                        srcDevice: 0,
                        srcArray: ptr::null_mut(),
                        srcPitch: 0,
                        dstMemoryType: CUmemorytype::CU_MEMORYTYPE_DEVICE,
                        dstHost: ptr::null_mut(),
                        dstDevice: self.input_device_ptr,
                        dstArray: ptr::null_mut(),
                        dstPitch: self.input_pitch,
                        WidthInBytes: (self.width * 4) as usize,
                        Height: self.height as usize,
                        ..Default::default()
                    };
                    if egl_frame.frame_type == CU_EGL_FRAME_TYPE_ARRAY {
                        copy_params.srcMemoryType = CUmemorytype::CU_MEMORYTYPE_ARRAY;
                        copy_params.srcArray = egl_frame.frame.p_array[0];
                    } else {
                        copy_params.srcMemoryType = CUmemorytype::CU_MEMORYTYPE_DEVICE;
                        copy_params.srcDevice = egl_frame.frame.p_pitch[0] as CUdeviceptr;
                        copy_params.srcPitch = egl_frame.pitch as usize;
                    }
                    // A fourcc without a packed NVENC equivalent keeps the surface's current
                    // registration; the copy still lands the bytes, as it always has.
                    if let Some(format) = fourcc_nvenc_format(fmt.code)
                        && let Err(e) = self.set_input_format(format)
                    {
                        (self.cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
                        return Err(e);
                    }
                    if (self.cuda.cuMemcpy2DAsync_v2)(&copy_params, ptr::null_mut())
                        != CUresult::CUDA_SUCCESS
                    {
                        (self.cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
                        return Err("Sanitization copy failed".into());
                    }
                    (self.mapped_input_buffer, self.input_format)
                }
            };

            // The convert reads the frame where it already is: the import itself when the driver
            // mapped it pitch-linear or handed back an array a texture covers, the session's own
            // surface when the frame was copied into it. No case adds a copy of its own.
            let swap = format == NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ABGR;
            let converted = match input {
                DmaBufInput::Copy => {
                    self.convert_packed(self.input_device_ptr, self.input_pitch, swap)
                }
                DmaBufInput::Direct { .. } if egl_frame.frame_type == CU_EGL_FRAME_TYPE_PITCH => {
                    self.convert_packed(
                        egl_frame.frame.p_pitch[0] as CUdeviceptr,
                        egl_frame.pitch as usize,
                        swap,
                    )
                }
                DmaBufInput::Direct { .. } => self.convert_texture(tex, swap),
            };
            let (mapped, format) = match converted {
                Ok(Some(converted)) => converted,
                Ok(None) => (mapped, format),
                Err(e) => {
                    (self.cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
                    return Err(e);
                }
            };

            let result = self.submit_frame(mapped, format, frame_number, force_idr);
            if result.is_err() {
                (self.cuda.cuStreamSynchronize)(ptr::null_mut());
            }
            (self.cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
            result
        }
    }

    /// Register the first plane of a mapped dmabuf frame with NVENC in place — as a pitch-linear
    /// CUDA device pointer or as a CUDA array, per `plane` — and map it as an input, under the
    /// already-current CUDA context. Either step failing falls back to `DmaBufInput::Copy` — the
    /// per-frame copy then serves that import for as long as it is cached, so a driver that declines
    /// the direct path costs one failed registration, not a failed frame.
    unsafe fn register_direct_input(
        &mut self,
        frame: &CUeglFrame,
        plane: DirectPlane,
        format: NV_ENC_BUFFER_FORMAT,
    ) -> DmaBufInput {
        let (resource_type, resource, pitch) = match plane {
            DirectPlane::Pitch(pitch) => (
                NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR,
                frame.frame.p_pitch[0],
                pitch,
            ),
            DirectPlane::Array(row_bytes) => (
                NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_CUDAARRAY,
                frame.frame.p_array[0] as *mut c_void,
                row_bytes,
            ),
        };
        let mut reg_res = NV_ENC_REGISTER_RESOURCE {
            version: sv(NvStruct::RegisterResource),
            resourceType: resource_type,
            width: self.width,
            height: self.height,
            resourceToRegister: resource,
            pitch,
            bufferFormat: format,
            bufferUsage: NV_ENC_BUFFER_USAGE::NV_ENC_INPUT_IMAGE,
            ..Default::default()
        };
        let st =
            (self.nvenc_funcs.nvEncRegisterResource.unwrap())(self.encoder_session, &mut reg_res);
        if st != NVENCSTATUS::NV_ENC_SUCCESS {
            eprintln!(
                "[NVENC] dmabuf plane registration ({plane:?}) refused ({st:?}); copying per frame."
            );
            return DmaBufInput::Copy;
        }
        let mut map_params = NV_ENC_MAP_INPUT_RESOURCE {
            version: sv(NvStruct::MapInputResource),
            registeredResource: reg_res.registeredResource,
            ..Default::default()
        };
        let st = (self.nvenc_funcs.nvEncMapInputResource.unwrap())(
            self.encoder_session,
            &mut map_params,
        );
        if st != NVENCSTATUS::NV_ENC_SUCCESS {
            (self.nvenc_funcs.nvEncUnregisterResource.unwrap())(
                self.encoder_session,
                reg_res.registeredResource,
            );
            eprintln!("[NVENC] dmabuf plane mapping refused ({st:?}); copying per frame.");
            return DmaBufInput::Copy;
        }
        DmaBufInput::Direct {
            registered: reg_res.registeredResource,
            mapped: map_params.mappedResource,
            format,
        }
    }

    /// Encode a host BGRA frame (B,G,R,A in memory, NVENC's word-ordered `ARGB` — the layout an
    /// XShm grab or the pixman framebuffer yields) through `encode_cpu_packed`.
    pub fn encode_cpu_argb(
        &mut self,
        argb: &[u8],
        src_stride: usize,
        frame_number: u64,
        crf: u32,
        force_idr: bool,
    ) -> Result<Vec<u8>, String> {
        self.encode_cpu_packed(argb, src_stride, false, frame_number, crf, force_idr)
    }

    /// Encode a host packed-pixel frame by uploading it straight into the packed input surface,
    /// with no CPU-side color conversion: the surface is either the chroma convert's source or,
    /// where the kernel did not load, NVENC's own conversion's; a host prepass would cost this
    /// path its copy-free property.
    ///
    /// `rgba_input` names the byte order — `false` for B,G,R,A (X11 XShm, the pixman framebuffer),
    /// `true` for R,G,B,A (a GLES readback) — and the input surface is registered with NVENC in
    /// that order (`ARGB` / `ABGR`, re-registered in place when it changes), so both arrive at the
    /// hardware CSC untouched. `src_stride` is the source row stride in bytes (`>= width*4`).
    /// Steps, under the pushed CUDA context after any pending QP change:
    ///
    /// 1. **Bounds-check** the source against `stride × (rows-1) + width*4`, erroring rather than
    ///    reading past a short buffer.
    /// 2. **Pin the source once**: unless pinning was disabled at init (`PIXELFLUX_NVENC_PIN=0`, read
    ///    once into `pin_uploads`), page-lock each distinct source base address via `pin_host_source`
    ///    so the upload is a direct DMA from the caller's buffer instead of a pageable copy staged
    ///    through a driver bounce buffer. The persistent, bounded shm / pool sources make this a
    ///    one-time bounded cost.
    /// 3. **Upload and submit**: `cuMemcpy2DAsync` the rows into the input surface on the default
    ///    stream honoring `src_stride`, then `submit_frame`. NVENC processes its input on that same
    ///    stream, so the upload is ordered before the encode without a host wait, and the blocking
    ///    bitstream lock inside `submit_frame` (or the stream sync on its error path) guarantees the
    ///    upload has finished reading `pixels` by the time this returns — the caller may reuse the
    ///    buffer immediately.
    pub fn encode_cpu_packed(
        &mut self,
        pixels: &[u8],
        src_stride: usize,
        rgba_input: bool,
        frame_number: u64,
        crf: u32,
        force_idr: bool,
    ) -> Result<Vec<u8>, String> {
        unsafe {
            self.reconfigure_if_needed(crf);
            let _ = (self.cuda.cuCtxPushCurrent_v2)(self.cuda_context);

            let width_bytes = (self.width * 4) as usize;
            let rows = self.height as usize;
            let needed = if rows == 0 {
                0
            } else {
                src_stride * (rows - 1) + width_bytes
            };
            if src_stride < width_bytes || pixels.len() < needed {
                (self.cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
                return Err(format!(
                    "packed buffer too small: len={} need>={} (stride={}, {}x{})",
                    pixels.len(),
                    needed,
                    src_stride,
                    self.width,
                    self.height
                ));
            }

            let format = if rgba_input {
                NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ABGR
            } else {
                NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB
            };
            // With the convert in place the packed surface is the kernel's source and needs no
            // registration of its own; NVENC is handed the NV12 the kernel writes.
            if self.csc.is_none()
                && let Err(e) = self.set_input_format(format)
            {
                (self.cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
                return Err(e);
            }

            if self.pin_uploads {
                self.pin_host_source(pixels.as_ptr() as usize, pixels.len());
            }

            let copy = CUDA_MEMCPY2D {
                srcMemoryType: CUmemorytype::CU_MEMORYTYPE_HOST,
                srcHost: pixels.as_ptr() as *const c_void,
                srcPitch: src_stride,
                dstMemoryType: CUmemorytype::CU_MEMORYTYPE_DEVICE,
                dstDevice: self.input_device_ptr,
                dstPitch: self.input_pitch,
                WidthInBytes: width_bytes,
                Height: rows,
                ..Default::default()
            };
            if (self.cuda.cuMemcpy2DAsync_v2)(&copy, ptr::null_mut()) != CUresult::CUDA_SUCCESS {
                (self.cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
                return Err("packed host->device upload failed".into());
            }

            let (mapped, submitted) =
                match self.convert_packed(self.input_device_ptr, self.input_pitch, rgba_input) {
                    Ok(Some(converted)) => converted,
                    Ok(None) => (self.mapped_input_buffer, self.input_format),
                    Err(e) => {
                        (self.cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
                        return Err(e);
                    }
                };
            let result = self.submit_frame(mapped, submitted, frame_number, force_idr);
            if result.is_err() {
                (self.cuda.cuStreamSynchronize)(ptr::null_mut());
            }
            self.upload_hash = None;
            if let Some(words) = self.upload_words.take()
                && let Some(Some(hash)) = self.band_hash.as_mut()
            {
                self.upload_hashes = hash.read(&self.cuda, words);
            }
            (self.cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
            result
        }
    }

    /// Run the chroma convert over an array-typed import through `tex`, answering the input
    /// NVENC should be handed. `None` where the session has no convert, or where the driver gave
    /// no texture for the import and NVENC's own conversion stands in, which a 10-bit session,
    /// initialized for the convert's surface, has none of.
    unsafe fn convert_texture(
        &self,
        tex: CUtexObject,
        rgba_input: bool,
    ) -> Result<Option<(NV_ENC_INPUT_PTR, NV_ENC_BUFFER_FORMAT)>, String> {
        match self.csc.as_ref() {
            Some(csc) if tex != 0 => {
                csc.run_texture(&self.cuda, tex, self.width, self.height, rgba_input)?;
                Ok(Some(csc.input()))
            }
            _ if self.bit_depth > 8 => {
                Err("the driver gave no texture for the import a 10-bit session converts".into())
            }
            _ => Ok(None),
        }
    }

    /// Run the chroma convert over a packed surface, answering the input NVENC should be
    /// handed, or `None` where the session has no convert and encodes the packed surface itself.
    /// The caller holds the CUDA context current.
    unsafe fn convert_packed(
        &self,
        src: CUdeviceptr,
        src_pitch: usize,
        rgba_input: bool,
    ) -> Result<Option<(NV_ENC_INPUT_PTR, NV_ENC_BUFFER_FORMAT)>, String> {
        match self.csc.as_ref() {
            Some(csc) => {
                csc.run(
                    &self.cuda,
                    src,
                    src_pitch,
                    self.width,
                    self.height,
                    rgba_input,
                )?;
                Ok(Some(csc.input()))
            }
            None => Ok(None),
        }
    }

    /// The geometry the session is currently initialized for.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// The geometry the session is currently initialized for.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Make this session's CUDA context current on the calling thread, so a capture source can
    /// allocate its frames in the very context the encoder reads them from — the whole basis of
    /// a zero-copy hand-over. Paired with [`NvencEncoder::pop_context`].
    pub(crate) fn push_context(&self) -> bool {
        unsafe { (self.cuda.cuCtxPushCurrent_v2)(self.cuda_context) == CUresult::CUDA_SUCCESS }
    }

    /// Give back the context [`NvencEncoder::push_context`] made current.
    pub(crate) fn pop_context(&self) {
        unsafe {
            (self.cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
        }
    }

    /// Drop the registration of an externally-owned input surface, under the pushed CUDA context.
    ///
    /// The caller owns the memory behind it, so this must run before that owner releases it: a
    /// registration outliving its buffer leaves the driver holding a mapping of freed video
    /// memory.
    pub fn release_external_input(&mut self) {
        if self.external_inputs.is_empty() {
            return;
        }
        unsafe {
            let _ = (self.cuda.cuCtxPushCurrent_v2)(self.cuda_context);
            self.unmap_external_inputs();
            let _ = (self.cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
        }
    }

    /// Unmap and unregister every external input, with the CUDA context already current.
    unsafe fn unmap_external_inputs(&mut self) {
        for ext in std::mem::take(&mut self.external_inputs) {
            unsafe { self.unmap_one_input(ext) };
        }
    }

    unsafe fn unmap_one_input(&self, ext: ExternalInput) {
        (self.nvenc_funcs.nvEncUnmapInputResource.unwrap())(self.encoder_session, ext.mapped);
        (self.nvenc_funcs.nvEncUnregisterResource.unwrap())(self.encoder_session, ext.registered);
    }

    /// The mapped NVENC input for an externally-owned device pointer, registering it in place the
    /// first time and reusing that registration for every later frame from the same buffer.
    ///
    /// A capture source hands back one buffer, or a few in turn (NvFBC alternates between two on
    /// recent drivers), for as long as its geometry holds, so the register and map cost is paid
    /// once per buffer rather than per frame. Up to `EXTERNAL_INPUTS` stay registered; a pitch,
    /// format or geometry that changes releases the registrations made for the old one.
    unsafe fn register_external_input(
        &mut self,
        device_ptr: CUdeviceptr,
        pitch: usize,
        format: NV_ENC_BUFFER_FORMAT,
    ) -> Result<NV_ENC_INPUT_PTR, String> {
        let (width, height) = (self.width, self.height);
        let fits = |e: &ExternalInput| {
            e.pitch == pitch && e.format == format && e.width == width && e.height == height
        };
        if let Some(ext) = self
            .external_inputs
            .iter()
            .find(|e| e.device_ptr == device_ptr && fits(e))
        {
            return Ok(ext.mapped);
        }
        let (kept, stale): (Vec<_>, Vec<_>) = std::mem::take(&mut self.external_inputs)
            .into_iter()
            .partition(fits);
        self.external_inputs = kept;
        for ext in stale {
            unsafe { self.unmap_one_input(ext) };
        }
        if self.external_inputs.len() >= EXTERNAL_INPUTS {
            let oldest = self.external_inputs.remove(0);
            unsafe { self.unmap_one_input(oldest) };
        }
        let mut reg_res = NV_ENC_REGISTER_RESOURCE {
            version: sv(NvStruct::RegisterResource),
            resourceType: NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR,
            width: self.width,
            height: self.height,
            resourceToRegister: device_ptr as *mut c_void,
            pitch: pitch as u32,
            bufferFormat: format,
            bufferUsage: NV_ENC_BUFFER_USAGE::NV_ENC_INPUT_IMAGE,
            ..Default::default()
        };
        let st =
            (self.nvenc_funcs.nvEncRegisterResource.unwrap())(self.encoder_session, &mut reg_res);
        if st != NVENCSTATUS::NV_ENC_SUCCESS {
            return Err(format!(
                "failed to register the captured frame as an NVENC input ({st:?})"
            ));
        }
        let mut map_params = NV_ENC_MAP_INPUT_RESOURCE {
            version: sv(NvStruct::MapInputResource),
            registeredResource: reg_res.registeredResource,
            ..Default::default()
        };
        let st = (self.nvenc_funcs.nvEncMapInputResource.unwrap())(
            self.encoder_session,
            &mut map_params,
        );
        if st != NVENCSTATUS::NV_ENC_SUCCESS {
            (self.nvenc_funcs.nvEncUnregisterResource.unwrap())(
                self.encoder_session,
                reg_res.registeredResource,
            );
            return Err(format!(
                "failed to map the captured frame as an NVENC input ({st:?})"
            ));
        }
        self.external_inputs.push(ExternalInput {
            device_ptr,
            pitch,
            width: self.width,
            height: self.height,
            format,
            registered: reg_res.registeredResource,
            mapped: map_params.mappedResource,
        });
        Ok(map_params.mappedResource)
    }

    /// Encode a frame that already lives in video memory, reading it exactly where it was
    /// produced.
    ///
    /// This is the zero-copy hand-over: `device_ptr` addresses packed pixels in this session's own
    /// CUDA context — the X11 NvFBC capture buffer the NVIDIA driver composited the screen into —
    /// so no upload or `cuMemcpy` stands between the screen and the bitstream. The chroma convert
    /// reads that buffer where it lies, as it does for every other input, and a session without
    /// one hands the packed surface to NVENC's own conversion instead, registered in place.
    /// `rgba` names the byte order (`false` for the B,G,R,A the driver's native format delivers),
    /// and `pitch` is the buffer's row stride in bytes.
    ///
    /// The caller must keep the buffer alive and unmodified until this returns; the blocking
    /// bitstream lock inside means the encoder has finished reading by then, so the next capture
    /// may overwrite it.
    pub fn encode_cuda_pitch(
        &mut self,
        device_ptr: CUdeviceptr,
        pitch: usize,
        rgba: bool,
        frame_number: u64,
        crf: u32,
        force_idr: bool,
    ) -> Result<Vec<u8>, String> {
        unsafe {
            self.reconfigure_if_needed(crf);
            // The pitch has to cover the session's rows whichever conversion reads them: the
            // kernel indexes by it, and NVENC's own registration requires the 4-byte alignment.
            if pitch < self.width as usize * 4 || !pitch.is_multiple_of(4) {
                return Err(format!(
                    "external input pitch {pitch} does not cover {}x{} at 4-byte alignment",
                    self.width, self.height
                ));
            }
            let _ = (self.cuda.cuCtxPushCurrent_v2)(self.cuda_context);
            let format = if rgba {
                NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ABGR
            } else {
                NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB
            };
            let submitted = match self.convert_packed(device_ptr, pitch, rgba) {
                Ok(Some(converted)) => Ok(converted),
                Ok(None) => self
                    .register_external_input(device_ptr, pitch, format)
                    .map(|m| (m, format)),
                Err(e) => Err(e),
            };
            let (mapped, submitted) = match submitted {
                Ok(pair) => pair,
                Err(e) => {
                    (self.cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
                    return Err(e);
                }
            };
            let result = self.submit_frame(mapped, submitted, frame_number, force_idr);
            if result.is_err() {
                (self.cuda.cuStreamSynchronize)(ptr::null_mut());
            }
            (self.cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
            result
        }
    }

    /// Hash the next host frame's bands of `rows` rows on the device as it encodes, for
    /// `take_upload_hashes` to read after: the change detection a Turbo frame's paint-over
    /// reads, for the cost of a kernel beside the encode instead of a CPU pass over the frame.
    /// False where the driver refuses the kernel, which leaves the hash to the caller.
    pub fn hash_next_upload(&mut self, rows: u32) -> bool {
        self.upload_hashes = None;
        self.upload_words = None;
        let available = unsafe {
            let _ = (self.cuda.cuCtxPushCurrent_v2)(self.cuda_context);
            let cuda = &self.cuda;
            let loaded = self
                .band_hash
                .get_or_insert_with(|| BandHash::new(cuda))
                .is_some();
            (self.cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
            loaded
        };
        self.upload_hash = available.then_some(rows);
        available
    }

    /// The band hashes of the host frame last coded where `hash_next_upload` asked for them.
    pub fn take_upload_hashes(&mut self) -> Option<Vec<u64>> {
        self.upload_hashes.take()
    }

    /// The hash of each band of `rows` rows of the `width`x`height` packed frame at
    /// `device_ptr` (`pitch` bytes a row, in this session's context), computed where the frame
    /// lies: the change detection of a capture that reports none, NvFBC's. None where the driver
    /// refuses the kernel.
    pub fn band_hashes(
        &mut self,
        device_ptr: CUdeviceptr,
        pitch: usize,
        width: u32,
        height: u32,
        rows: u32,
    ) -> Option<Vec<u64>> {
        if pitch < width as usize * 4 {
            return None;
        }
        unsafe {
            let _ = (self.cuda.cuCtxPushCurrent_v2)(self.cuda_context);
            let cuda = &self.cuda;
            let hashes = self
                .band_hash
                .get_or_insert_with(|| BandHash::new(cuda))
                .as_mut()
                .and_then(|h| h.run(cuda, device_ptr, pitch, width, height, rows));
            (self.cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
            hashes
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every codec's arm declares the BT.709 matrix the session converts with, in both chroma
    /// formats and with no device involved. A stream whose pixels and signal disagree shifts
    /// every saturated color on the client, and only a GPU would otherwise report it.
    #[test]
    fn every_codec_declares_the_conversion_matrix() {
        for codec in [Codec::H264, Codec::H265, Codec::Av1] {
            for fullcolor in [false, true] {
                let mut config = NV_ENC_CONFIG {
                    version: sv(NvStruct::Config),
                    ..Default::default()
                };
                NvencEncoder::configure_codec(
                    &mut config,
                    codec,
                    fullcolor,
                    8,
                    nvenc_level(codec, 1280, 720, 60, 0, true),
                    1,
                    0,
                    &NvencTuning::default(),
                );
                let got = unsafe {
                    match codec {
                        Codec::H265 => {
                            config
                                .encodeCodecConfig
                                .hevcConfig
                                .hevcVUIParameters
                                .colourMatrix
                        }
                        Codec::Av1 => config.encodeCodecConfig.av1Config.matrixCoefficients,
                        _ => {
                            config
                                .encodeCodecConfig
                                .h264Config
                                .h264VUIParameters
                                .colourMatrix
                        }
                    }
                };
                assert_eq!(
                    got as u32,
                    NV_ENC_VUI_MATRIX_COEFFS::NV_ENC_VUI_MATRIX_COEFFS_BT709 as u32,
                    "{codec:?} 4:4:4={fullcolor}"
                );
            }
        }
    }

    #[test]
    fn anchors_require_the_negotiated_api_and_device_capacity() {
        for acked in [false, true] {
            assert_eq!(anchor_count(Codec::Av1, 12, true, Some(6), 4, acked), 0);
            assert_eq!(anchor_count(Codec::Av1, 13, false, Some(6), 4, acked), 0);
            assert_eq!(anchor_count(Codec::Av1, 13, true, None, 4, acked), 0);
            assert_eq!(anchor_count(Codec::Av1, 13, true, Some(0), 4, acked), 0);
            assert_eq!(anchor_count(Codec::Av1, 13, true, Some(6), 2, acked), 0);
            assert_eq!(anchor_count(Codec::Av1, 13, true, Some(1), 3, acked), 1);
            for api in [11, 12, 13] {
                assert_eq!(
                    anchor_count(Codec::H265, api, true, Some(8), 4, acked),
                    ANCHORS
                );
                for codec in [Codec::H264, Codec::H265] {
                    assert_eq!(anchor_count(codec, api, false, Some(8), 4, acked), 0);
                    assert_eq!(anchor_count(codec, api, true, Some(0), 4, acked), 0);
                    assert_eq!(anchor_count(codec, api, true, Some(8), 2, acked), 0);
                }
            }
        }
        assert_eq!(anchor_count(Codec::Av1, 13, true, Some(6), 4, false), 1);
        assert_eq!(
            anchor_count(Codec::Av1, 13, true, Some(6), 4, true),
            ANCHORS
        );
        for api in [11, 12, 13] {
            for acked in [false, true] {
                assert_eq!(anchor_count(Codec::H264, api, true, Some(8), 4, acked), 1);
            }
        }
        // No room for two: the one an unacknowledged session keeps.
        assert_eq!(anchor_count(Codec::Av1, 13, true, Some(8), 3, true), 1);
        assert_eq!(anchor_count(Codec::Av1, 13, true, Some(1), 4, true), 1);
    }

    /// AV1's persistent reference must be enabled in its own codec arm; merely keeping an
    /// anchor in ReferenceWindow would report a dependency the device does not retain.
    #[test]
    fn av1_anchor_configuration_matches_the_reference_window() {
        for anchors in [0, 1] {
            let mut config = NV_ENC_CONFIG::default();
            NvencEncoder::configure_codec(
                &mut config,
                Codec::Av1,
                false,
                8,
                13,
                AV1_REFERENCES,
                anchors,
                &NvencTuning::default(),
            );
            let av1 = unsafe { config.encodeCodecConfig.av1Config };
            assert_eq!(av1.enableLTR(), anchors as u32);
            assert_eq!(av1.ltrNumFrames, anchors as u32);
            assert_eq!(av1.maxNumRefFramesInDPB, AV1_REFERENCES);
        }
    }

    /// Frames an anchor's settling leaves out of every prediction take the bands they held with
    /// them: the band the anchor holds reaches back over theirs, and an anchor that holds none
    /// loses them to the cleanup; bands before them stay as they are.
    #[test]
    fn bands_left_out_of_the_predictions_are_coded_again() {
        let sweep = || VecDeque::from([(10, 0.0, 0.1), (11, 0.1, 0.2), (12, 0.2, 0.3)]);
        let mut log = sweep();
        assert_eq!(
            reach_back(&mut log, 11, Some((0.3, 0.4)), true),
            (Some((0.1, 0.4)), false)
        );
        assert_eq!(log, VecDeque::from([(10, 0.0, 0.1)]));
        assert_eq!(
            reach_back(&mut sweep(), 11, None, false),
            (None, true),
            "a frame holding nothing loses them"
        );
        assert_eq!(
            reach_back(&mut sweep(), 11, None, true),
            (None, false),
            "a frame held whole codes them"
        );
        assert_eq!(
            reach_back(&mut sweep(), 13, Some((0.3, 0.4)), true),
            (Some((0.3, 0.4)), false),
            "no band among the frames left out"
        );
    }

    /// A band's share of the picture lands on the blocks it covers, rounded outward, so the bands
    /// of a sweep leave no block between them, and nowhere else.
    #[test]
    fn a_band_covers_its_share_of_the_blocks() {
        let mut map = Vec::new();
        fill_band_map(&mut map, Codec::H264, 1920, 1080, (0.0, 1.0 / 64.0), -33);
        assert_eq!(map.len(), 120 * 68);
        assert_eq!(map.iter().filter(|&&d| d == -33).count(), 128);
        assert!(map[..128].iter().all(|&d| d == -33) && map[128..].iter().all(|&d| d == 0));
        fill_band_map(&mut map, Codec::H265, 1920, 1080, (0.5, 0.75), -80);
        assert_eq!(map.len(), 60 * 34);
        assert!(map[..1020].iter().all(|&d| d == 0) && map[1530..].iter().all(|&d| d == 0));
        assert!(
            map[1020..1530].iter().all(|&d| d == -51),
            "the delta is bounded to the quantizer range"
        );
        fill_band_map(&mut map, Codec::Av1, 1920, 1080, (0.0, 1.0), -211);
        assert_eq!(map.len(), 30 * 17);
        assert!(
            map.iter().all(|&d| d == -128),
            "an AV1 delta is bounded to what the map holds"
        );
        let covered = |a: f64, b: f64| {
            let mut m = Vec::new();
            fill_band_map(&mut m, Codec::H264, 1280, 720, (a, b), -1);
            m.iter().map(|&d| d != 0).collect::<Vec<bool>>()
        };
        let (x, y) = (covered(0.0, 0.3), covered(0.3, 1.0));
        assert!(
            x.iter().zip(&y).all(|(a, b)| *a || *b),
            "adjacent bands leave no block out"
        );
    }
}

#[cfg(test)]
pub(crate) mod gpu_tests {
    use super::*;
    use crate::encoders::reference::REFERENCE_FRAMES;

    /// Test helper: H.264 full-frame capture settings at `w×h`, `fps`, CRF 25.
    fn settings(w: i32, h: i32, fps: f64) -> CaptureSettings {
        CaptureSettings {
            width: w,
            height: h,
            codec: Codec::H264,
            target_fps: fps,
            video_crf: 25,
            ..Default::default()
        }
    }

    /// Test helper: a session for checks that hand it a fresh buffer every frame, which it pins
    /// none of: a registration outlives the buffer it page-locked, and a later buffer at an
    /// overlapping address then fails to upload, or uploads the pages of the freed one, where a
    /// capture's persistent shm is released on every reshape.
    fn host_session(s: &CaptureSettings) -> Result<NvencEncoder, String> {
        NvencEncoder::new(s, ptr::null()).map(|mut enc| {
            enc.pin_uploads = false;
            enc
        })
    }

    /// Test helper: `host_session`, or `None` where the device has no engine for the codec (AV1
    /// before Ada, HEVC on Kepler) or for its 4:4:4 (Kepler), which the caller skips.
    fn codec_session(s: &CaptureSettings) -> Option<NvencEncoder> {
        match host_session(s) {
            Ok(enc) => Some(enc),
            Err(e) if e.contains("engine") => {
                println!("{:?}: {e}", s.codec);
                None
            }
            Err(e) => panic!("{:?} NVENC init: {e}", s.codec),
        }
    }

    /// Test helper: a `w×h` BGRA frame filled with a hashed gradient (offset by `seed`) so
    /// the content has structure and encodes are non-trivial.
    fn frame(w: usize, h: usize, seed: u8) -> Vec<u8> {
        let mut f = vec![0u8; w * h * 4];
        for (i, px) in f.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let v = ((i as u32).wrapping_mul(2654435761) >> 24) as u8;
            px[0] = v.wrapping_add(seed);
            px[1] = v ^ seed;
            px[2] = seed;
            px[3] = 255;
        }
        f
    }

    /// Test helper: hand the session's `encode_config` to `nvEncReconfigureEncoder` as it stands,
    /// for checks that vary one rate-control field, with the driver's answer.
    fn reconfigure_raw(enc: &mut NvencEncoder) -> (NVENCSTATUS, String) {
        unsafe {
            let status = enc.reconfigure(false, false);
            let detail = if status == NVENCSTATUS::NV_ENC_SUCCESS {
                "accepted".to_string()
            } else {
                last_error(&enc.nvenc_funcs, enc.encoder_session)
            };
            (status, detail)
        }
    }

    /// Test helper: how far apart, in mean luma, the last picture of `frames` decodes with and
    /// without the frames at the `lost` indices.
    fn apart_without(codec: Codec, frames: &[Vec<u8>], lost: std::ops::Range<usize>) -> f64 {
        use crate::webcam::decode::{Decoder as _, VideoDecoder};
        let (mut whole, mut lossy) = (
            VideoDecoder::new(codec).unwrap(),
            VideoDecoder::new(codec).unwrap(),
        );
        for (i, f) in frames.iter().enumerate() {
            assert!(whole.decode(f).expect("decode"), "{codec:?} frame {i}");
            if !lost.contains(&i) {
                assert!(
                    lossy.decode(f).expect("decode past the loss"),
                    "{codec:?} frame {i}"
                );
            }
        }
        luma_apart(&whole.frame().unwrap(), &lossy.frame().unwrap())
    }

    /// Test helper: the mean luma distance between two decoded pictures.
    fn luma_apart(
        a: &crate::webcam::convert::I420View<'_>,
        b: &crate::webcam::convert::I420View<'_>,
    ) -> f64 {
        a.y.chunks(a.y_stride)
            .zip(b.y.chunks(b.y_stride))
            .take(a.height)
            .flat_map(|(ra, rb)| {
                ra[..a.width]
                    .iter()
                    .zip(&rb[..a.width])
                    .map(|(&x, &y)| (x as f64 - y as f64).abs())
            })
            .sum::<f64>()
            / (a.width * a.height) as f64
    }

    /// Test helper: the mean luma distance between the last picture decoded from every frame and
    /// the last one decoded from the frames at the `keep` indices alone; None where those left the
    /// last one undecodable.
    fn apart_keeping(codec: Codec, frames: &[Vec<u8>], keep: &[usize]) -> Option<f64> {
        use crate::webcam::decode::{Decoder as _, VideoDecoder};
        let (mut whole, mut part) = (
            VideoDecoder::new(codec).unwrap(),
            VideoDecoder::new(codec).unwrap(),
        );
        let mut got = false;
        for (i, f) in frames.iter().enumerate() {
            let _ = whole.decode(f);
            if keep.contains(&i) {
                got = part.decode(f).unwrap_or(false) && i == frames.len() - 1;
            }
        }
        if !got {
            return None;
        }
        Some(luma_apart(&whole.frame()?, &part.frame()?))
    }

    /// Test helper: the gradient frame with a 256x256 block of another gradient moved `step`
    /// blocks along its top rows, the frames of a steady desktop-like sequence.
    fn moving_frame(w: usize, h: usize, step: usize) -> Vec<u8> {
        let mut f = frame(w, h, 10);
        let block = frame(256, 256, 200);
        let x0 = (step * 64) % (w - 256);
        for row in 0..256.min(h) {
            let dst = (row * w + x0) * 4;
            f[dst..dst + 256 * 4].copy_from_slice(&block[row * 256 * 4..(row + 1) * 256 * 4]);
        }
        f
    }

    /// Test helper: a NAL unit with its emulation-prevention bytes removed.
    fn rbsp(nal: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(nal.len());
        let mut zeros = 0usize;
        for &b in nal {
            if zeros >= 2 && b == 3 {
                zeros = 0;
                continue;
            }
            zeros = if b == 0 { zeros + 1 } else { 0 };
            out.push(b);
        }
        out
    }

    /// Test helper: luma PSNR of one decoded access unit against the BGRA frame it encodes, the
    /// source taken through the limited-range BT.709 luma the session converts with.
    fn luma_psnr(
        dec: &mut crate::webcam::decode::VideoDecoder,
        pkt: &[u8],
        src: &[u8],
        w: usize,
        h: usize,
    ) -> f64 {
        use crate::webcam::decode::Decoder;
        assert!(
            dec.decode(&pkt[VIDEO_HEADER_LEN..]).expect("decode"),
            "no picture"
        );
        let v = dec.frame().expect("decoded frame");
        let mut se = 0f64;
        for y in 0..h {
            for x in 0..w {
                let p = &src[(y * w + x) * 4..(y * w + x) * 4 + 3];
                let luma = crate::encoders::chroma_siting::ycbcr(
                    [p[2], p[1], p[0]].map(f64::from),
                    crate::encoders::chroma_siting::BT709,
                )[0];
                let d = v.y[y * v.y_stride + x] as f64 - luma;
                se += d * d;
            }
        }
        let mse = se / (w * h) as f64;
        if mse == 0.0 {
            99.0
        } else {
            10.0 * (255.0f64 * 255.0 / mse).log10()
        }
    }

    /// Test helper: one CBR bench row. Encodes `seq` on `enc`, its first frame as the warm-up key
    /// frame, and prints the achieved rate at `fps`, the smallest and largest frame, and the luma
    /// PSNR of every decoded frame against its source.
    fn cbr_row(
        label: &str,
        enc: &mut NvencEncoder,
        codec: Codec,
        seq: &[&Vec<u8>],
        w: usize,
        h: usize,
        fps: usize,
    ) {
        use crate::webcam::decode::VideoDecoder;
        let n = seq.len() - 1;
        let first = enc
            .encode_cpu_packed(seq[0], w * 4, false, 0, 25, true)
            .expect("warm-up");
        let mut pkts: Vec<Vec<u8>> = Vec::with_capacity(n);
        per_frame(label, n, |i| {
            pkts.push(
                enc.encode_cpu_packed(seq[1 + i], w * 4, false, 1 + i as u64, 25, false)
                    .expect("encode"),
            );
        });
        let sizes: Vec<usize> = pkts
            .iter()
            .map(|p| p.len().saturating_sub(VIDEO_HEADER_LEN))
            .collect();
        let bytes: usize = sizes.iter().sum();
        let mut dec = VideoDecoder::new(codec).expect("decoder");
        luma_psnr(&mut dec, &first, seq[0], w, h);
        let psnr: Vec<f64> = pkts
            .iter()
            .enumerate()
            .map(|(i, p)| luma_psnr(&mut dec, p, seq[1 + i], w, h))
            .collect();
        println!(
            "    {} kbps, frames {}..{} kbit, luma PSNR {:.1} dB mean, {:.1} dB worst",
            bytes * 8 * fps / n / 1000,
            sizes.iter().min().unwrap() * 8 / 1000,
            sizes.iter().max().unwrap() * 8 / 1000,
            psnr.iter().sum::<f64>() / n as f64,
            psnr.iter().cloned().fold(f64::INFINITY, f64::min),
        );
    }

    /// Test helper: read the big-endian width/height (bytes 6-9) from the wire header.
    fn wire_dims(pkt: &[u8]) -> (u16, u16) {
        (
            u16::from_be_bytes([pkt[6], pkt[7]]),
            u16::from_be_bytes([pkt[8], pkt[9]]),
        )
    }

    /// End-to-end in-place resize on a real GPU: encode 720p, grow to 1080p and verify the
    /// first post-resize frame is an IDR (steady frames are P), shrink to 480p, exercise the
    /// rejection cases (beyond headroom, chroma flip, RC-mode flip) and confirm the session still
    /// encodes afterward, and optionally dump a decodable stream. Ignored by default; run with
    /// `cargo test gpu_ -- --ignored --nocapture --test-threads=1` — building NVENC sessions
    /// concurrently races in the driver and intermittently faults, so the GPU set runs serially.
    #[test]
    #[ignore]
    fn gpu_resolution_reconfigure_roundtrip() {
        let mut s = settings(1280, 720, 60.0);
        let t0 = std::time::Instant::now();
        let mut enc = host_session(&s).expect("NVENC init");
        let init_ms = t0.elapsed().as_secs_f64() * 1000.0;

        let mut stream: Vec<u8> = Vec::new();
        let f720 = frame(1280, 720, 10);
        for i in 0..5u64 {
            let pkt = enc
                .encode_cpu_argb(&f720, 1280 * 4, i, 25, i == 0)
                .expect("encode 720p");
            assert_eq!(wire_dims(&pkt), (1280, 720));
            stream.extend_from_slice(&pkt[VIDEO_HEADER_LEN..]);
        }

        assert!(
            !enc.reconfigure_resolution(&s)
                .expect("same-size reconfigure"),
            "unchanged dimensions must not reset the session"
        );
        let pkt = enc
            .encode_cpu_argb(&f720, 1280 * 4, 5, 25, false)
            .expect("encode after same-size reconfigure");
        assert_eq!(
            pkt[1] & 0x0f,
            FRAME_DELTA,
            "the stream continues without an IDR at unchanged dimensions"
        );
        assert!(
            pkt.len() > VIDEO_HEADER_LEN,
            "a locked bitstream carries the encoded picture"
        );
        stream.extend_from_slice(&pkt[VIDEO_HEADER_LEN..]);

        s.width = 1920;
        s.height = 1080;
        let t1 = std::time::Instant::now();
        assert!(enc.reconfigure_resolution(&s).expect("grow reconfigure"));
        let grow_ms = t1.elapsed().as_secs_f64() * 1000.0;
        let f1080 = frame(1920, 1080, 40);
        let pkt = enc
            .encode_cpu_argb(&f1080, 1920 * 4, 6, 25, false)
            .expect("encode 1080p");
        assert_eq!(pkt[0], 0x04);
        assert_eq!(
            pkt[1] & 0x0f,
            FRAME_KEY,
            "first frame after a resize must be an IDR"
        );
        assert_eq!(wire_dims(&pkt), (1920, 1080));
        assert!(
            pkt.len() > VIDEO_HEADER_LEN,
            "a locked bitstream carries the encoded picture"
        );
        stream.extend_from_slice(&pkt[VIDEO_HEADER_LEN..]);
        for i in 7..10u64 {
            let pkt = enc
                .encode_cpu_argb(&f1080, 1920 * 4, i, 25, false)
                .expect("encode 1080p");
            assert_eq!(
                pkt[1] & 0x0f,
                FRAME_DELTA,
                "steady frames after the IDR are P frames"
            );
            stream.extend_from_slice(&pkt[VIDEO_HEADER_LEN..]);
        }

        s.width = 640;
        s.height = 480;
        let t2 = std::time::Instant::now();
        assert!(enc.reconfigure_resolution(&s).expect("shrink reconfigure"));
        let shrink_ms = t2.elapsed().as_secs_f64() * 1000.0;
        let f480 = frame(640, 480, 70);
        let pkt = enc
            .encode_cpu_argb(&f480, 640 * 4, 10, 25, false)
            .expect("encode 480p");
        assert_eq!(pkt[1] & 0x0f, FRAME_KEY);
        assert_eq!(wire_dims(&pkt), (640, 480));
        stream.extend_from_slice(&pkt[VIDEO_HEADER_LEN..]);

        s.width = 4100;
        s.height = 2400;
        assert!(enc.reconfigure_resolution(&s).is_err(), "beyond headroom");
        s.width = 640;
        s.height = 480;
        s.video_fullcolor = true;
        assert!(enc.reconfigure_resolution(&s).is_err(), "chroma flip");
        s.video_fullcolor = false;
        s.video_cbr_mode = true;
        assert!(enc.reconfigure_resolution(&s).is_err(), "RC mode flip");
        s.video_cbr_mode = false;
        let pkt = enc
            .encode_cpu_argb(&f480, 640 * 4, 11, 25, false)
            .expect("session survives rejected reconfigures");
        stream.extend_from_slice(&pkt[VIDEO_HEADER_LEN..]);

        println!(
            "init={init_ms:.1}ms grow(720p->1080p)={grow_ms:.1}ms shrink(1080p->480p)={shrink_ms:.1}ms"
        );
        if let Ok(path) = std::env::var("NVENC_TEST_DUMP") {
            std::fs::write(&path, &stream).unwrap();
            println!("wrote {} bytes to {path}", stream.len());
        }
    }

    /// On a real GPU: every codec the device lists an engine for comes up on host frames
    /// without loading EGL, frames it emits carry the codec's wire id and a kind the
    /// bitstream agrees with, decode back
    /// to the painted picture, and a key frame forced mid-stream starts a fresh decoder on
    /// its own; a codec the device lacks (AV1 before Ada) is refused with a message naming
    /// it. HEVC 4:4:4 is taken where the device carries it, AV1 never quietly. Ignored by
    /// default.
    #[test]
    #[ignore]
    fn gpu_codec_sessions_encode_and_decode() {
        use crate::encoders::codec::{
            FRAME_DELTA, FRAME_KEY, av1_is_key, h264_frame_type, h265_frame_type, parse_video_type,
        };
        use crate::webcam::decode::{Decoder, VideoDecoder};
        let (w, h) = (1280usize, 720usize);
        for codec in [Codec::H264, Codec::H265, Codec::Av1] {
            let mut s = settings(w as i32, h as i32, 60.0);
            s.codec = codec;
            let mut enc = match host_session(&s) {
                Ok(enc) => enc,
                Err(e) => {
                    println!("{codec:?}: {e}");
                    assert!(
                        e.contains("engine"),
                        "a missing codec must be refused as such: {e}"
                    );
                    continue;
                }
            };
            assert_eq!(enc.codec(), codec);
            assert!(enc.egl.is_none(), "a session on host frames loads no EGL");
            let mut dec = VideoDecoder::new(codec).expect("decoder");
            for i in 0..6u64 {
                let src = frame(w, h, 10 + i as u8);
                let pkt = enc
                    .encode_cpu_argb(&src, w * 4, i, 25, i == 0)
                    .expect("encode");
                let (wire_codec, kind) = parse_video_type(pkt[1]).expect("video type byte");
                assert_eq!(wire_codec, codec);
                assert_eq!(
                    kind,
                    if i == 0 { FRAME_KEY } else { FRAME_DELTA },
                    "{codec:?} frame {i}"
                );
                let payload = &pkt[VIDEO_HEADER_LEN..];
                if codec != Codec::Av1 {
                    assert!(
                        payload.starts_with(&[0, 0, 0, 1]),
                        "{codec:?} frame {i}: the bitstream starts right after the header"
                    );
                }
                let read = match codec {
                    Codec::H264 => h264_frame_type(payload),
                    Codec::H265 => h265_frame_type(payload),
                    _ => {
                        if av1_is_key(payload) {
                            FRAME_KEY
                        } else {
                            FRAME_DELTA
                        }
                    }
                };
                assert_eq!(
                    read, kind,
                    "{codec:?} frame {i}: the bitstream disagrees with pictureType"
                );
                assert!(
                    dec.decode(payload).expect("decode"),
                    "{codec:?} frame {i} decoded nothing"
                );
                let f = dec.frame().unwrap();
                assert_eq!((f.width, f.height), (w, h));
            }
            let key = enc
                .encode_cpu_argb(&frame(w, h, 99), w * 4, 6, 25, true)
                .expect("forced key");
            assert_eq!(parse_video_type(key[1]), Some((codec, FRAME_KEY)));
            let mut fresh = VideoDecoder::new(codec).expect("decoder");
            assert!(
                fresh.decode(&key[VIDEO_HEADER_LEN..]).expect("decode"),
                "{codec:?}: a forced key frame must decode alone"
            );

            let mut full = s.clone();
            full.video_fullcolor = true;
            let Some(enc) = codec_session(&full) else {
                continue;
            };
            if codec.fullcolor() {
                println!("{codec:?} 4:4:4: {}", enc.is_fullcolor());
            } else {
                assert!(!enc.is_fullcolor(), "{codec:?} never carries 4:4:4");
            }
        }
    }

    /// On a real GPU: a 10-bit request is coded at 10 bits in each format the probe lists them
    /// for and at 8 in every other, through the convert that writes the 10-bit surface, and an
    /// in-place resize keeps it. The decoders here take 8-bit streams alone, so a 10-bit key
    /// frame is one they refuse for its depth. Ignored by default.
    #[test]
    #[ignore]
    fn gpu_ten_bit_follows_the_device() {
        use crate::webcam::decode::{Decoder, VideoDecoder};
        let (w, h) = (1280usize, 720usize);
        for (codec, formats) in probe_codecs(0).expect("probe") {
            for fullcolor in [false, codec.fullcolor()] {
                let mut s = settings(w as i32, h as i32, 60.0);
                s.codec = codec;
                s.video_fullcolor = fullcolor;
                s.video_bit_depth = 10;
                let Some(mut enc) = codec_session(&s) else {
                    continue;
                };
                let carried = formats.ten_bit[(fullcolor && formats.fullcolor) as usize];
                println!("{codec:?} 4:4:4 {fullcolor}: {}-bit", enc.bit_depth());
                assert_eq!(enc.bit_depth(), if carried { 10 } else { 8 }, "{codec:?}");
                assert_eq!(
                    enc.csc.as_ref().map(|c| c.layout),
                    ConvertLayout::of(enc.is_fullcolor(), enc.bit_depth()),
                    "{codec:?}"
                );
                let key = enc
                    .encode_cpu_argb(&frame(w, h, 1), w * 4, 0, 25, true)
                    .expect("key frame");
                let decoded = VideoDecoder::new(codec)
                    .expect("decoder")
                    .decode(&key[VIDEO_HEADER_LEN..]);
                if carried {
                    let refusal = format!("{:?}", decoded.expect_err("an 8-bit decoder"));
                    assert!(refusal.contains("bit depth"), "{codec:?}: {refusal}");
                } else if !enc.is_fullcolor() {
                    assert!(decoded.expect("decode"), "{codec:?}");
                }
                s.width = 1920;
                s.height = 1080;
                enc.reconfigure_resolution(&s).expect("resize");
                assert_eq!(enc.bit_depth(), if carried { 10 } else { 8 });
                enc.encode_cpu_argb(&frame(1920, 1080, 2), 1920 * 4, 1, 25, false)
                    .expect("a frame after the resize");
                s.video_bit_depth = 8;
                assert_eq!(
                    enc.reconfigure_resolution(&s).is_err(),
                    carried,
                    "{codec:?}: a depth change rebuilds the session"
                );
            }
        }
    }

    /// A frame a client lost is left out of the device's predictions: the next frame predicts
    /// from the newest frame before it and names it, a decoder that never saw the lost frames
    /// decodes it as one that saw everything does, the stream declares the decoded picture
    /// buffer the level admits, and an in-place resize redeclares it: a loss as deep as the new
    /// buffer's recent frames reach is still predicted past, a deeper one from the key frame the
    /// resize forced where an anchor is kept, and losing the last reference costs a key frame. A
    /// device that cannot invalidate a reference tracks none and says so. Ignored by default.
    #[test]
    #[ignore]
    fn gpu_predicts_past_a_lost_frame() {
        use crate::encoders::reference::Reference;
        use crate::encoders::sps::h264_max_num_ref_frames;
        let (w, h) = (1280usize, 720usize);
        for codec in [Codec::H264, Codec::H265, Codec::Av1] {
            let mut s = settings(w as i32, h as i32, 60.0);
            s.codec = codec;
            s.omit_stripe_headers = true;
            let mut enc = match host_session(&s) {
                Ok(enc) => enc,
                Err(e) => {
                    println!("{codec:?}: {e}");
                    continue;
                }
            };
            let encode = |enc: &mut NvencEncoder, i: usize, w: usize, h: usize| {
                let out = enc
                    .encode_cpu_argb(&moving_frame(w, h, i), w * 4, i as u64, 25, i == 0)
                    .expect("encode");
                (out, enc.last_reference())
            };
            let (first, reference) = encode(&mut enc, 0, w, h);
            if reference == Reference::Untracked {
                println!(
                    "{codec:?}: this device cannot invalidate a reference, so nothing is tracked"
                );
                assert!(!enc.invalidate_reference(0));
                continue;
            }
            assert_eq!(reference, Reference::None);
            if codec == Codec::H264 {
                assert_eq!(
                    h264_max_num_ref_frames(&first),
                    Some(REFERENCE_FRAMES),
                    "the SPS declares the DPB"
                );
            }
            let mut frames = vec![first];
            for i in 1..8 {
                let (out, reference) = encode(&mut enc, i, w, h);
                assert_eq!(
                    reference,
                    Reference::Frame(i as u16 - 1),
                    "{codec:?} frame {i}"
                );
                frames.push(out);
            }
            assert!(
                enc.invalidate_reference(5),
                "{codec:?}: the device refused the invalidation"
            );
            let (out, reference) = encode(&mut enc, 8, w, h);
            // AV1's four-slot DPB leaves three recent pictures beside its one anchor.
            // The oldest recent picture (4) is already gone, so it predicts from anchor 0.
            let before_loss = if codec == Codec::Av1
                && enc
                    .references
                    .as_ref()
                    .is_some_and(ReferenceWindow::anchored)
            {
                0
            } else {
                4
            };
            assert_eq!(reference, Reference::Frame(before_loss), "{codec:?}");
            frames.push(out);
            let (out, reference) = encode(&mut enc, 9, w, h);
            assert_eq!(reference, Reference::Frame(8), "{codec:?}");
            frames.push(out);
            let off = apart_without(codec, &frames, 5..8);
            println!("{codec:?}: frame 9 without frames 5-7 is {off:.3} off the complete decode");
            assert!(
                off < 0.5,
                "{codec:?}: the decoder that lost frames 5-7 shows frame 9 {off:.2} off the one that saw them"
            );
            // A grow to a level admitting fewer frames declares the smaller buffer at the IDR the
            // resize forces.
            s.width = 1920;
            s.height = 1080;
            assert!(
                enc.reconfigure_resolution(&s).expect("in-place grow"),
                "{codec:?}"
            );
            let dpb = enc.dpb as usize;
            let anchored = enc.references.as_ref().unwrap().anchored();
            let (out, reference) = encode(&mut enc, 10, 1920, 1080);
            assert_eq!(reference, Reference::None);
            if codec == Codec::H264 {
                assert_eq!(
                    h264_max_num_ref_frames(&out),
                    Some(4),
                    "1080p at level 4.2 admits four"
                );
            }
            let mut frames = vec![out];
            for i in 11..=10 + dpb {
                let (out, reference) = encode(&mut enc, i, 1920, 1080);
                assert_eq!(
                    reference,
                    Reference::Frame(i as u16 - 1),
                    "{codec:?} frame {i}"
                );
                frames.push(out);
            }
            assert!(
                enc.invalidate_reference(12),
                "{codec:?}: the device refused the invalidation"
            );
            let (out, reference) = encode(&mut enc, 11 + dpb, 1920, 1080);
            assert_eq!(
                reference,
                Reference::Frame(if anchored { 10 } else { 11 }),
                "{codec:?}: use the newest reference still held before the loss"
            );
            frames.push(out);
            let (out, reference) = encode(&mut enc, 12 + dpb, 1920, 1080);
            assert_eq!(reference, Reference::Frame(11 + dpb as u16), "{codec:?}");
            frames.push(out);
            let off = apart_without(codec, &frames, 2..dpb + 1);
            println!(
                "{codec:?}: at 1080p, frame {} without frames 12-{} is {off:.3} off the complete decode",
                12 + dpb,
                10 + dpb
            );
            assert!(
                off < 0.5,
                "{codec:?}: the decoder that lost frames 12-{} shows {off:.2} off the one that saw them",
                10 + dpb
            );
            assert!(enc.invalidate_reference(11));
            assert_eq!(
                encode(&mut enc, 13 + dpb, 1920, 1080).1,
                if anchored {
                    Reference::Frame(10)
                } else {
                    Reference::None
                },
                "{codec:?}: losing frame 11 requires the older anchor or a key frame"
            );
            assert!(enc.invalidate_reference(10));
            assert_eq!(
                encode(&mut enc, 14 + dpb, 1920, 1080).1,
                if anchored {
                    Reference::None
                } else {
                    Reference::Frame(13 + dpb as u16)
                },
                "{codec:?}: losing the anchor requires a key frame; a report before a new key frame is ignored"
            );
        }
    }

    /// A report for a frame an earlier invalidation already left out keeps the chain predicted
    /// since: lose 80 and 81, recover from 79, and the report for 81 that arrives after 98 lets 99
    /// predict from 98 rather than cost a key frame; a loss in the new chain still invalidates
    /// it. The stream without the lost frames decodes as the complete one does. Ignored by
    /// default.
    #[test]
    #[ignore]
    fn gpu_a_covered_loss_keeps_the_recovered_chain() {
        use crate::encoders::reference::Reference;
        let (w, h) = (1920usize, 1080usize);
        for codec in [Codec::H264, Codec::H265, Codec::Av1] {
            let mut s = settings(w as i32, h as i32, 60.0);
            s.codec = codec;
            s.omit_stripe_headers = true;
            let mut enc = match host_session(&s) {
                Ok(enc) => enc,
                Err(e) => {
                    println!("{codec:?}: {e}");
                    continue;
                }
            };
            let mut frames = Vec::new();
            for i in 0..=107usize {
                let (lost, expect) = match i {
                    82 => (Some(80), Some(79)),
                    99 => (Some(81), Some(98)),
                    107 => (Some(106), Some(105)),
                    _ => (None, None),
                };
                if let Some(lost) = lost {
                    assert!(enc.invalidate_reference(lost), "{codec:?}: lost {lost}");
                }
                frames.push(
                    enc.encode_cpu_argb(&moving_frame(w, h, i), w * 4, i as u64, 25, i == 0)
                        .expect("encode"),
                );
                if i == 0 && enc.last_reference() == Reference::Untracked {
                    break;
                }
                if let Some(from) = expect {
                    assert_eq!(
                        enc.last_reference(),
                        Reference::Frame(from),
                        "{codec:?} {i}"
                    );
                }
            }
            if enc.last_reference() == Reference::Untracked {
                println!("{codec:?}: this device cannot invalidate a reference");
                continue;
            }
            let received: Vec<usize> = (0..frames.len())
                .filter(|i| !matches!(i, 80 | 81 | 106))
                .collect();
            let off = apart_keeping(codec, &frames, &received);
            assert!(
                off.is_some_and(|x| x < 0.5),
                "{codec:?}: without frames 80, 81 and 106 the last frame is {off:?} off"
            );
            println!("{codec:?}: frame 99 predicts from 98 after the covered report, {off:?} off");
        }
    }

    /// A held refresh of the whole picture past `HELD_REFRESH_LIMIT_S` of a constant rate is coded
    /// again to fit `HELD_KEY_BUDGET_S`, and one under it stands. The frame coded again predicts
    /// from the frame before the first attempt, which the device leaves out as it does a lost
    /// frame: a decoder that never sees the attempt decodes what follows as one that saw it does,
    /// and a loss reported for the frame sent in its place leaves that frame out. Ignored by
    /// default.
    #[test]
    #[ignore]
    fn gpu_a_held_refresh_past_its_limit_is_coded_again() {
        use crate::encoders::reference::Reference;
        let (w, h) = (1280usize, 720usize);
        let still = frame(w, h, 10);
        let encode = |enc: &mut NvencEncoder, i: u64, held: Option<u32>| {
            if let Some(crf) = held {
                enc.hold_quantizer(crf, None);
            }
            let out = enc
                .encode_cpu_argb(&still, w * 4, i, 25, i == 0)
                .expect("encode");
            (out, enc.last_reference())
        };
        for codec in [Codec::H264, Codec::H265, Codec::Av1] {
            let mut s = settings(w as i32, h as i32, 60.0);
            s.codec = codec;
            s.omit_stripe_headers = true;
            s.video_cbr_mode = true;
            s.video_bitrate_kbps = 50_000;
            let mut enc = match host_session(&s) {
                Ok(enc) => enc,
                Err(e) => {
                    println!("{codec:?}: {e}");
                    continue;
                }
            };
            let mut frames: Vec<_> = (0..4).map(|i| encode(&mut enc, i, None).0).collect();
            if enc.last_reference() == Reference::Untracked {
                println!("{codec:?}: this device cannot invalidate a reference");
                continue;
            }
            // An attempt within the limit of this rate, taken back as the session takes back one
            // past it.
            let (attempt, _) = encode(&mut enc, 4, Some(20));
            assert!(enc.retract_last(), "{codec:?}: the device refused");
            frames.push(attempt);
            let (out, reference) = encode(&mut enc, 4, Some(40));
            assert_eq!(reference, Reference::Frame(3), "{codec:?}");
            frames.push(out);
            let (out, reference) = encode(&mut enc, 5, None);
            assert_eq!(reference, Reference::Frame(4), "{codec:?}");
            frames.push(out);
            let off = apart_without(codec, &frames, 4..5);
            assert!(
                off < 0.5,
                "{codec:?}: without the attempt the frames after it decode {off:.2} off"
            );
            assert!(enc.invalidate_reference(4), "{codec:?}");
            let (out, reference) = encode(&mut enc, 6, None);
            assert!(
                matches!(reference, Reference::Frame(id) if id < 4),
                "{codec:?}: {reference:?} after losing the frame sent in place of the attempt"
            );
            frames.push(out);
            let lost = apart_without(codec, &frames, 4..7);
            assert!(
                lost < 0.5,
                "{codec:?}: past the loss the frame decodes {lost:.2} off"
            );
            // Past `HELD_REFRESH_LIMIT_S` of the target the session codes such a frame again on its
            // own (AV1 on 595.91.07 caps a held frame at a second of the target itself); under it,
            // the frame stands.
            for (kbps, first) in [(100, 7u64), (1000, 11)] {
                s.video_bitrate_kbps = kbps;
                assert!(enc.reconfigure_rate(&s), "{codec:?}");
                for i in first..first + 3 {
                    encode(&mut enc, i, None);
                }
                let pts = |enc: &NvencEncoder| enc.references.as_ref().map_or(0, |r| r.next_pts());
                let before = pts(&enc);
                let (out, reference) = encode(&mut enc, first + 3, Some(20));
                assert_eq!(reference, Reference::Frame(first as u16 + 2), "{codec:?}");
                let spent = out.len() as f64 / (kbps as f64 * 1000.0 / 8.0);
                let coded = pts(&enc) - before;
                println!(
                    "{codec:?}: a held frame at {kbps} kbps goes out at {} bytes, {spent:.2} s of the target, encoded {coded} times",
                    out.len()
                );
                assert!(spent <= crate::encoders::HELD_REFRESH_LIMIT_S, "{codec:?}");
                if kbps == 1000 {
                    assert_eq!(coded, 1, "{codec:?}: a frame under the limit stands");
                }
            }
        }
    }

    /// A cleanup frame that comes out past its cap (`cap`) is taken back and coded again: a held
    /// band in a shorter one, which `refit` reports, and a frame of the rate control's at the
    /// coarsest quantizer, refining nothing. The frame sent in place of the attempt predicts from
    /// the one before it, and the stream decodes as one without the attempt; a frame within its
    /// cap stands. Ignored by default.
    #[test]
    #[ignore]
    fn gpu_a_cleanup_frame_past_its_cap_is_coded_again() {
        use crate::encoders::reference::Reference;
        use crate::webcam::decode::{Decoder as _, VideoDecoder};
        let (w, h) = (1280usize, 720usize);
        let still = frame(w, h, 10);
        for codec in [Codec::H264, Codec::H265, Codec::Av1] {
            let mut s = settings(w as i32, h as i32, 60.0);
            s.codec = codec;
            s.omit_stripe_headers = true;
            s.video_cbr_mode = true;
            s.video_bitrate_kbps = 2000;
            let mut enc = match host_session(&s) {
                Ok(enc) => enc,
                Err(e) => {
                    println!("{codec:?}: {e}");
                    continue;
                }
            };
            let encode = |enc: &mut NvencEncoder, i: u64| {
                enc.encode_cpu_argb(&still, w * 4, i, 25, i == 0)
                    .expect("encode")
            };
            let mut frames: Vec<_> = (0..4).map(|i| encode(&mut enc, i)).collect();
            if enc.last_reference() == Reference::Untracked {
                println!("{codec:?}: this device cannot invalidate a reference");
                continue;
            }
            enc.hold_quantizer(10, Some((0.0, 0.5)));
            enc.cap(4000);
            frames.push(encode(&mut enc, 4));
            let end = enc.refit().expect("the band taken back");
            println!(
                "{codec:?}: a band to 0.5 coded again to {end:.3}, {} bytes",
                frames[4].len()
            );
            assert!(end > 0.0 && end < 0.5, "{codec:?}: {end}");
            assert_eq!(enc.last_reference(), Reference::Frame(3), "{codec:?}");
            enc.cap(1);
            frames.push(encode(&mut enc, 5));
            assert_eq!(
                enc.refit(),
                Some(0.0),
                "{codec:?}: the rate control's frame taken back"
            );
            assert_eq!(enc.last_reference(), Reference::Frame(4), "{codec:?}");
            enc.cap(usize::MAX);
            frames.push(encode(&mut enc, 6));
            assert_eq!(
                enc.refit(),
                None,
                "{codec:?}: a frame within its cap stands"
            );
            let mut decoder = VideoDecoder::new(codec).unwrap();
            for (i, f) in frames.iter().enumerate() {
                assert!(decoder.decode(f).expect("decode"), "{codec:?} frame {i}");
            }
        }
    }

    /// Two consumers share a session told which frames both hold: one takes every frame, the other
    /// misses stretches, everything but the anchors and, where there are two, then everything,
    /// and once it has room reports the frame its missing run starts with. Of two anchors, the
    /// newest both hold stays pinned while the schedule marks the other, which predicts from the
    /// frame before it (both hold none of the two newest), so the one behind misses the anchors
    /// too; the one behind decodes each frame it is sent as the other does, the first after its
    /// report predicting from the anchor it holds; and no frame after the first is a key frame.
    /// H.264 keeps one anchor, a frame every forty-eight, and marks none while the one behind
    /// holds no recent frame, which then predicts past from the one it holds. Ignored by default.
    #[test]
    #[ignore]
    fn gpu_acknowledged_anchors_carry_a_consumer_left_behind() {
        use crate::encoders::codec::parse_video_type;
        use crate::encoders::reference::Reference;
        use crate::webcam::decode::{Decoder as _, VideoDecoder};
        let (w, h) = (1280usize, 720usize);
        // (frames it has no room for, anchors too, the frame its report goes ahead of, the anchor
        // the frame after the report predicts from), for two anchors and for one
        let pinned = [(13..=40, false, 42, 12u16), (61..=100, true, 102, 60)];
        let alone = [(49..=90, false, 92, 48u16), (97..=150, false, 152, 96)];
        for codec in [Codec::H264, Codec::H265, Codec::Av1] {
            let mut s = settings(w as i32, h as i32, 60.0);
            s.codec = codec;
            s.acknowledge_references = true;
            let mut enc = match host_session(&s) {
                Ok(enc) => enc,
                Err(e) => {
                    println!("{codec:?}: {e}");
                    continue;
                }
            };
            let anchors = enc
                .references
                .as_ref()
                .map_or(0, ReferenceWindow::anchor_count);
            let (stretches, period, last) = match anchors {
                ANCHORS => (&pinned, 12, 120),
                1 => (&alone, 48, 160),
                _ => {
                    println!("{codec:?}: this device or API keeps no long-term reference");
                    continue;
                }
            };
            let (mut whole, mut part) = (
                VideoDecoder::new(codec).unwrap(),
                VideoDecoder::new(codec).unwrap(),
            );
            let mut holds = Vec::new();
            let mut run = None;
            let (mut worst, mut sizes) = (0f64, (0usize, 0usize, 0usize, 0usize));
            for i in 0..=last {
                let stretch = stretches.iter().find(|t| t.0.contains(&i));
                if let Some(&(_, _, _, from)) = stretches.iter().find(|t| t.2 == i) {
                    let first = run.take().expect("a missing run");
                    assert!(enc.take_report(ReferenceReport::Lost(first)), "{codec:?}");
                    assert_eq!(
                        enc.references
                            .as_ref()
                            .and_then(ReferenceWindow::newest_valid)
                            .map(|f| f.0),
                        Some(from),
                        "{codec:?}: {first} reported lost before {i}"
                    );
                }
                let out = enc
                    .encode_cpu_argb(&moving_frame(w, h, i), w * 4, i as u64, 25, i == 0)
                    .expect("encode");
                let (_, kind) = parse_video_type(out[1]).expect("video type byte");
                assert_eq!(
                    kind == FRAME_KEY,
                    i == 0,
                    "{codec:?}: frame {i} is kind {kind}"
                );
                let reference = enc.last_reference();
                let can = match reference {
                    Reference::None => true,
                    Reference::Frame(r) => holds[r as usize],
                    Reference::Untracked => false,
                };
                let anchor = out[1] & FRAME_ANCHOR != 0;
                if anchor {
                    assert!(
                        can,
                        "{codec:?}: anchor {i} predicts from {reference:?}, held by one"
                    );
                }
                assert_eq!(
                    anchor,
                    i > 0 && i % period == 0 && can,
                    "{codec:?}: frame {i} predicting from {reference:?}"
                );
                let sent = can && stretch.is_none_or(|t| anchor && !t.1);
                let bits = &out[VIDEO_HEADER_LEN..];
                assert!(whole.decode(bits).unwrap_or(false), "{codec:?}: frame {i}");
                if sent {
                    assert!(
                        part.decode(bits).unwrap_or(false),
                        "{codec:?}: frame {i} sent"
                    );
                    let off = luma_apart(&whole.frame().unwrap(), &part.frame().unwrap());
                    assert!(off < 0.5, "{codec:?}: frame {i} sent is {off:.3} off");
                    worst = worst.max(off);
                    run = None;
                    enc.acknowledge_reference(i as u16, true);
                } else if can {
                    run = Some(i as u16);
                }
                holds.push(sent);
                if i > 0 {
                    let bucket = if anchor { &mut sizes.2 } else { &mut sizes.0 };
                    *bucket += out.len();
                    if anchor { sizes.3 += 1 } else { sizes.1 += 1 }
                }
            }
            println!(
                "{codec:?}: worst {worst:.3} apart; {} B a frame, {} B an anchor ({} anchors)",
                sizes.0 / sizes.1.max(1),
                sizes.2 / sizes.3.max(1),
                sizes.3
            );
        }
    }

    /// With the second anchor lost, a loss of the two newest frames is predicted past from the
    /// newest frame the device still holds, and the window names it: on AV1 one past the recent
    /// frames beside two anchors, held in the long-term slot the lost anchor lends. A decoder
    /// given the frames up to it, and none after, decodes the next frame as one given every frame
    /// does. Ignored by default.
    #[test]
    #[ignore]
    fn gpu_a_lost_anchor_lends_its_slot_to_the_recent_frames() {
        let (w, h) = (1280usize, 720usize);
        for codec in [Codec::H264, Codec::H265, Codec::Av1] {
            let mut s = settings(w as i32, h as i32, 60.0);
            s.codec = codec;
            s.acknowledge_references = true;
            s.omit_stripe_headers = true;
            let mut enc = match host_session(&s) {
                Ok(enc) => enc,
                Err(e) => {
                    println!("{codec:?}: {e}");
                    continue;
                }
            };
            if !enc
                .references
                .as_ref()
                .is_some_and(ReferenceWindow::anchored)
            {
                println!("{codec:?}: this device or API keeps no long-term reference");
                continue;
            }
            let mut frames = Vec::new();
            let mut from = 0;
            for i in 0..=22usize {
                match i {
                    16 => assert!(enc.take_report(ReferenceReport::Lost(12)), "{codec:?}"),
                    21 => assert!(enc.take_report(ReferenceReport::Lost(19)), "{codec:?}"),
                    _ => {}
                }
                let out = enc
                    .encode_cpu_argb(&moving_frame(w, h, i), w * 4, i as u64, 25, i == 0)
                    .expect("encode");
                if i <= 11 || (16..=17).contains(&i) {
                    enc.acknowledge_reference(i as u16, true);
                }
                frames.push(out);
                if i == 21 {
                    let Reference::Frame(r) = enc.last_reference() else {
                        panic!("{codec:?}: {:?} past the loss", enc.last_reference());
                    };
                    from = r as usize;
                }
            }
            if codec == Codec::Av1 {
                assert_eq!(from, 18, "{codec:?}: past the loss of 19 and 20");
            }
            let keep: Vec<usize> = (0..=11).chain(16..=from).chain([21, 22]).collect();
            let off = apart_keeping(codec, &frames, &keep);
            println!("{codec:?}: past the loss from {from}, {off:?} apart");
            assert!(
                off.is_some_and(|off| off < 0.5),
                "{codec:?}: given the frames up to {from}, frame 22 decodes {off:?} apart"
            );
        }
    }

    /// A loss deeper than the recent frames is predicted past from the newest anchor before it,
    /// the one the device predicts from, and decodes as the complete stream does; one before every
    /// anchor costs a key frame. An H.264 session, and an AV1 one on an API with AV1 long-term
    /// references, keeps one anchor, a frame every forty-eight, an H.265 one two, a frame every
    /// twelve by turns, and on Pascal reaches one recent frame past its buffer. A CBR rate lowered
    /// between the anchor and the loss keeps the anchor. Ignored by default.
    #[test]
    #[ignore]
    fn gpu_predicts_past_a_loss_deeper_than_the_recent_frames() {
        use crate::encoders::reference::Reference;
        // (codec, frames sent after the key frame, first frame lost, the anchor predicted from, an
        // older anchor the device must not have taken, CBR lowered from 20 to 8 Mbit/s as the lost
        // frame is encoded), at 1080p and, for the frames `split_mode` splits, at 4K
        let cases = [
            (Codec::H264, 60u16, 50u16, Some(48u16), Some(0u16), false),
            (Codec::H264, 60, 57, Some(48), Some(0), false),
            (Codec::H264, 80, 64, Some(48), Some(0), false),
            (Codec::H264, 80, 51, Some(48), Some(0), false),
            (Codec::H264, 80, 64, Some(48), Some(0), true),
            (Codec::H264, 60, 40, None, None, false),
            (Codec::Av1, 60, 50, Some(48), Some(0), false),
            (Codec::Av1, 60, 57, Some(48), Some(0), false),
            (Codec::Av1, 80, 64, Some(48), Some(0), false),
            (Codec::Av1, 80, 64, Some(48), Some(0), true),
            (Codec::Av1, 60, 40, None, None, false),
            (Codec::H265, 30, 28, Some(27), Some(26), false),
            (Codec::H265, 30, 27, Some(24), Some(12), false),
            (Codec::H265, 40, 37, Some(36), Some(24), false),
            (Codec::H265, 40, 30, Some(24), Some(12), false),
            (Codec::H265, 58, 40, Some(36), Some(24), false),
            (Codec::H265, 58, 37, Some(36), Some(24), false),
            (Codec::H265, 40, 20, None, None, false),
        ];
        let split = [
            (Codec::Av1, 60u16, 50u16, Some(48u16), Some(0u16), false),
            (Codec::H265, 30, 28, Some(27), Some(26), false),
            (Codec::H265, 40, 30, Some(24), Some(12), false),
        ];
        let fmt = |v: Option<f64>| v.map_or("no picture".to_string(), |x| format!("{x:.3}"));
        let sized = (cases.into_iter().map(|c| (1920usize, 1080usize, c)))
            .chain(split.into_iter().map(|c| (3840, 2160, c)));
        for (w, h, (codec, sent, lost, from, older, retarget)) in sized {
            let mut s = settings(w as i32, h as i32, 60.0);
            s.codec = codec;
            s.omit_stripe_headers = true;
            if retarget {
                s.video_cbr_mode = true;
                s.video_bitrate_kbps = 20_000;
            }
            let mut enc = match host_session(&s) {
                Ok(enc) => enc,
                Err(e) => {
                    println!("{codec:?}: {e}");
                    continue;
                }
            };
            if codec == Codec::Av1
                && !enc
                    .references
                    .as_ref()
                    .is_some_and(ReferenceWindow::anchored)
            {
                println!("{codec:?}: this device or API keeps no long-term reference");
                continue;
            }
            let mut frames = Vec::new();
            for i in 0..=sent as usize {
                if retarget && i == lost as usize {
                    s.video_bitrate_kbps = 8000;
                    assert!(
                        enc.reconfigure_rate(&s),
                        "{codec:?}: the rate change was refused"
                    );
                }
                frames.push(
                    enc.encode_cpu_argb(&moving_frame(w, h, i), w * 4, i as u64, 25, i == 0)
                        .expect("encode"),
                );
                if i == 0 && enc.last_reference() == Reference::Untracked {
                    break;
                }
            }
            if enc.last_reference() == Reference::Untracked {
                println!("{codec:?}: this device cannot invalidate a reference");
                continue;
            }
            let normal = frames[sent as usize].len();
            assert!(
                enc.invalidate_reference(lost),
                "{codec:?}: the device refused the invalidation"
            );
            let next = sent as usize + 1;
            frames.push(
                enc.encode_cpu_argb(&moving_frame(w, h, next), w * 4, next as u64, 25, false)
                    .expect("encode"),
            );
            let reference = enc.last_reference();
            let keeping = |upto: usize| (0..=upto).chain([next]).collect::<Vec<_>>();
            let without_lost: Vec<usize> = (0..=next)
                .filter(|&i| i < lost as usize || i == next)
                .collect();
            println!(
                "{codec:?} {w}x{h}: frame {lost} of {sent} lost, frame {next} predicts from {reference:?}, {} B against {normal} B",
                frames[next].len()
            );
            // The frame after an anchor is marked predicts from the anchor itself.
            if let Some(anchor) = from.filter(|&a| a > 0 && a < sent).map(usize::from) {
                let skipped: Vec<usize> = (0..=anchor + 1).filter(|&i| i != anchor).collect();
                let off = apart_keeping(codec, &frames[..=anchor + 1], &skipped);
                assert!(
                    off.is_none_or(|x| x >= 0.5),
                    "{codec:?}: frame {} decodes without {anchor} ({})",
                    anchor + 1,
                    fmt(off)
                );
            }
            match from {
                Some(anchor) => {
                    assert_eq!(
                        reference,
                        Reference::Frame(anchor),
                        "{codec:?}: lost {lost}"
                    );
                    let off = apart_keeping(codec, &frames, &without_lost);
                    assert!(
                        off.is_some_and(|x| x < 0.5),
                        "{codec:?}: without {lost}-{sent} the frame is {} off",
                        fmt(off)
                    );
                    let off = apart_keeping(codec, &frames, &keeping(anchor as usize));
                    assert!(
                        off.is_some_and(|x| x < 0.5),
                        "{codec:?}: up to the anchor {anchor} the frame is {} off",
                        fmt(off)
                    );
                    if let Some(older) = older {
                        let off = apart_keeping(codec, &frames, &keeping(older as usize));
                        let past_buffer = codec == Codec::H265
                            && anchor == sent - (enc.dpb - ANCHORS as u32) as u16;
                        if past_buffer && off.is_some_and(|x| x < 0.5) {
                            println!(
                                "{codec:?}: lost {lost}, the device predicts from before {anchor}, the frame past its buffer"
                            );
                        } else {
                            assert!(
                                off.is_none_or(|x| x >= 0.5),
                                "{codec:?}: the frame decodes from anchor {older} alone ({})",
                                fmt(off)
                            );
                        }
                    }
                }
                None => assert_eq!(
                    reference,
                    Reference::None,
                    "{codec:?}: lost {lost}, before every anchor"
                ),
            }
        }
    }

    /// A session opened at 1080p and resized in place to 720p keeps the buffer it declared, which
    /// the driver never raises, and still predicts past a loss six frames deep from the key frame
    /// the resize forced, its first anchor, decoding as the complete stream does. Ignored by
    /// default.
    #[test]
    #[ignore]
    fn gpu_a_shrink_predicts_past_a_loss_deeper_than_its_buffer() {
        use crate::encoders::reference::Reference;
        use crate::encoders::sps::h264_max_num_ref_frames;
        for codec in [Codec::H264, Codec::H265] {
            let mut s = settings(1920, 1080, 60.0);
            s.codec = codec;
            s.omit_stripe_headers = true;
            let mut enc = match host_session(&s) {
                Ok(enc) => enc,
                Err(e) if codec == Codec::H265 => {
                    println!("{codec:?}: {e}");
                    continue;
                }
                Err(e) => panic!("{codec:?}: {e}"),
            };
            let encode = |enc: &mut NvencEncoder, i: usize, w: usize, h: usize| {
                let out = enc
                    .encode_cpu_argb(&moving_frame(w, h, i), w * 4, i as u64, 25, i == 0)
                    .expect("encode");
                (out, enc.last_reference())
            };
            let (first, reference) = encode(&mut enc, 0, 1920, 1080);
            if reference == Reference::Untracked {
                println!("{codec:?}: this device cannot invalidate a reference");
                continue;
            }
            let declared = enc.dpb;
            if codec == Codec::H264 {
                assert_eq!(
                    h264_max_num_ref_frames(&first),
                    Some(4),
                    "1080p at level 4.2 admits four"
                );
            }
            (s.width, s.height) = (1280, 720);
            assert!(
                enc.reconfigure_resolution(&s).expect("in-place shrink"),
                "{codec:?}"
            );
            assert_eq!(
                enc.dpb, declared,
                "{codec:?}: the driver never raises the buffer"
            );
            let mut frames = Vec::new();
            for i in 1..=8 {
                let (out, reference) = encode(&mut enc, i, 1280, 720);
                let want = if i == 1 {
                    Reference::None
                } else {
                    Reference::Frame(i as u16 - 1)
                };
                assert_eq!(reference, want, "{codec:?} frame {i}");
                frames.push(out);
            }
            assert!(
                enc.invalidate_reference(2),
                "{codec:?}: the device refused the invalidation"
            );
            let (out, reference) = encode(&mut enc, 9, 1280, 720);
            assert_eq!(
                reference,
                Reference::Frame(1),
                "{codec:?}: a loss six deep, from the resize's key frame"
            );
            frames.push(out);
            let off = apart_without(codec, &frames, 1..8);
            println!(
                "{codec:?}: at 720p on a {declared}-frame buffer, frame 9 without frames 2-8 is {off:.3} off the complete decode"
            );
            assert!(
                off < 0.5,
                "{codec:?}: the decoder that lost frames 2-8 shows {off:.2} off the one that saw them"
            );
        }
    }

    /// On a real GPU, print what a loss costs the picture at CBR: the frames from one `depth`
    /// frames deep on never reach the decoder, as a receiver's frame buffer holds back what
    /// predicts from a lost frame, the loss is reported, and the recovery frame's size and the
    /// luma PSNR of it and the fourteen frames after it are measured against their sources, over
    /// losses at scattered phases; depth 0 is the same frames with nothing lost. Prints
    /// measurements to quote rather than asserting. Ignored by default.
    #[test]
    #[ignore]
    fn gpu_bench_loss_recovery() {
        use crate::encoders::reference::Reference;
        use crate::webcam::decode::{Decoder as _, VideoDecoder};
        let (w, h) = (1920usize, 1080usize);
        for codec in [Codec::H264, Codec::H265] {
            let mut s = settings(w as i32, h as i32, 60.0);
            s.codec = codec;
            s.video_cbr_mode = true;
            s.video_bitrate_kbps = 8000;
            for depth in [0usize, 1, 2, 3, 5, 8, 12] {
                let mut enc = host_session(&s).expect("init");
                let mut dec = VideoDecoder::new(codec).unwrap();
                let mut pending: std::collections::VecDeque<Vec<u8>> = Default::default();
                let mut rng = 0x2545_f491_4f6c_dd1du64 ^ depth as u64;
                let (mut sizes, mut first, mut after, mut keys) =
                    (Vec::new(), Vec::new(), Vec::new(), 0);
                let mut i = 0usize;
                for event in 0..10 {
                    rng ^= rng << 13;
                    rng ^= rng >> 7;
                    rng ^= rng << 17;
                    let gap = if event == 0 {
                        60
                    } else {
                        20 + (rng % 37) as usize
                    };
                    for _ in 0..gap {
                        let out = enc
                            .encode_cpu_argb(&moving_frame(w, h, i), w * 4, i as u64, 25, i == 0)
                            .expect("encode");
                        pending.push_back(out);
                        i += 1;
                        while pending.len() > depth + 1 {
                            let out = pending.pop_front().unwrap();
                            dec.decode(&out[VIDEO_HEADER_LEN..]).expect("decode");
                        }
                    }
                    // The newest `depth + 1` frames, the lost one and those after it, never arrive.
                    if depth == 0 {
                        for out in pending.drain(..) {
                            dec.decode(&out[VIDEO_HEADER_LEN..]).expect("decode");
                        }
                    } else {
                        pending.clear();
                        enc.invalidate_reference((i - 1 - depth) as u16);
                    }
                    let mut row = Vec::new();
                    for k in 0..15 {
                        let src = moving_frame(w, h, i);
                        let out = enc
                            .encode_cpu_argb(&src, w * 4, i as u64, 25, false)
                            .expect("encode");
                        if k == 0 {
                            sizes.push(out.len() - VIDEO_HEADER_LEN);
                            keys += usize::from(enc.last_reference() == Reference::None);
                        }
                        row.push(luma_psnr(&mut dec, &out, &src, w, h));
                        i += 1;
                    }
                    first.push(row[0]);
                    after.push(row.iter().sum::<f64>() / row.len() as f64);
                }
                sizes.sort();
                let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
                println!(
                    "RECOVERY {codec:?} depth {depth}: {keys}/10 key frames, answer {} B median, \
                     answer PSNR {:.2} dB mean ({:.2} worst), with the 14 after {:.2} dB mean",
                    sizes[sizes.len() / 2],
                    mean(&first),
                    first.iter().cloned().fold(f64::INFINITY, f64::min),
                    mean(&after),
                );
            }
        }
    }

    /// NVENC counts H.264's `frame_num` in 256 values and the stream in 65536 (`WideFrameNum`): a
    /// loss covering frame 256, where NVENC's own count wraps, is predicted past rather than
    /// answered with a key frame, and the decoder that lost it shows the next frame as one that
    /// saw it does. Ignored by default.
    #[test]
    #[ignore]
    fn gpu_predicts_past_a_loss_at_the_devices_frame_num_wrap() {
        use crate::encoders::reference::Reference;
        use crate::encoders::sps::h264_frame_num_range;
        let (w, h) = (1280usize, 720usize);
        let mut s = settings(w as i32, h as i32, 60.0);
        s.omit_stripe_headers = true;
        let mut enc = host_session(&s).expect("H.264 session");
        let encode = |enc: &mut NvencEncoder, i: usize| {
            let out = enc
                .encode_cpu_argb(&moving_frame(w, h, i), w * 4, i as u64, 25, i == 0)
                .expect("encode");
            (out, enc.last_reference())
        };
        let (first, reference) = encode(&mut enc, 0);
        if reference == Reference::Untracked {
            println!("this device cannot invalidate a reference, so nothing is tracked");
            return;
        }
        let range = h264_frame_num_range(&first).expect("the key frame carries the SPS") as usize;
        assert_eq!(range, 65536, "NVENC's 256 values, a byte wider");
        let mut frames = vec![first];
        for i in 1..=258 {
            frames.push(encode(&mut enc, i).0);
        }
        assert!(
            enc.invalidate_reference(256),
            "the device's wrap frame is reported lost"
        );
        let (out, reference) = encode(&mut enc, 259);
        assert!(
            matches!(reference, Reference::Frame(r) if r < 256),
            "{reference:?} past the loss"
        );
        frames.push(out);
        let off = apart_without(Codec::H264, &frames, 256..259);
        assert!(
            off < 0.5,
            "past the loss at the wrap the frame decodes {off:.2} off"
        );
    }

    /// On a real GPU, a CBR session resized 720p→1080p folds the new bitrate into the resize
    /// reconfigure (asserts `averageBitRate` updated to 8 Mbit/s) and the first post-resize frame is
    /// an IDR at the new dimensions. Ignored by default.
    #[test]
    #[ignore]
    fn gpu_resolution_reconfigure_cbr() {
        let mut s = settings(1280, 720, 60.0);
        s.video_cbr_mode = true;
        s.video_bitrate_kbps = 4000;
        let mut enc = host_session(&s).expect("NVENC init");
        let f720 = frame(1280, 720, 10);
        for i in 0..3u64 {
            enc.encode_cpu_argb(&f720, 1280 * 4, i, 25, i == 0)
                .expect("encode 720p");
        }
        s.width = 1920;
        s.height = 1080;
        s.video_bitrate_kbps = 8000;
        enc.reconfigure_resolution(&s).expect("cbr resize+rate");
        assert_eq!(enc.encode_config.rcParams.averageBitRate, 8_000_000);
        let f1080 = frame(1920, 1080, 40);
        let pkt = enc
            .encode_cpu_argb(&f1080, 1920 * 4, 3, 25, false)
            .expect("encode 1080p");
        assert_eq!(pkt[1] & 0x0f, FRAME_KEY);
        assert_eq!(wire_dims(&pkt), (1920, 1080));
    }

    /// A session told to keep one reference frame (`video_reference_frames`) declares a decoded
    /// picture buffer of one, H.264 and H.265 alike, and keeps no anchor; AV1 keeps its eight.
    /// Ignored by default.
    #[test]
    #[ignore]
    fn gpu_sessions_keep_the_reference_frames_they_are_given() {
        use crate::encoders::sps::h264_max_num_ref_frames;
        for codec in [Codec::H264, Codec::H265, Codec::Av1] {
            let mut s = settings(1280, 720, 60.0);
            s.codec = codec;
            s.omit_stripe_headers = true;
            s.acknowledge_references = true;
            s.video_reference_frames = 1;
            let mut enc = match host_session(&s) {
                Ok(enc) => enc,
                Err(e) => {
                    println!("{codec:?}: {e}");
                    continue;
                }
            };
            let key = enc
                .encode_cpu_argb(&frame(1280, 720, 10), 1280 * 4, 0, 25, true)
                .expect("encode");
            let anchors = enc
                .references
                .as_ref()
                .map_or(0, ReferenceWindow::anchor_count);
            match codec {
                Codec::Av1 => assert_eq!(enc.dpb, AV1_REFERENCES, "AV1"),
                _ => {
                    assert_eq!(enc.dpb, 1, "{codec:?}");
                    assert_eq!(anchors, 0, "{codec:?}");
                }
            }
            if codec == Codec::H264 {
                assert_eq!(h264_max_num_ref_frames(&key), Some(1));
            }
        }
    }

    /// Every H.264 session bounds reordering at zero, 4:2:0 and 4:4:4, and so does the IDR an
    /// in-place resize forces, which re-declares the stream. Ignored by default.
    #[test]
    #[ignore]
    fn gpu_h264_bounds_reordering_at_zero() {
        use crate::encoders::sps::fixtures::assert_no_reorder;
        for fullcolor in [false, true] {
            let mut s = settings(1920, 1080, 60.0);
            s.video_fullcolor = fullcolor;
            s.omit_stripe_headers = true;
            let mut enc = match host_session(&s) {
                Ok(enc) => enc,
                Err(e) => {
                    println!("fullcolor {fullcolor}: {e}");
                    continue;
                }
            };
            let key = enc
                .encode_cpu_argb(&frame(1920, 1080, 10), 1920 * 4, 0, 25, true)
                .expect("encode 1080p");
            assert_no_reorder(&key, &format!("NVENC fullcolor {fullcolor} 1080p"));
            s.width = 1280;
            s.height = 720;
            if enc.reconfigure_resolution(&s).expect("in-place resize") {
                let key = enc
                    .encode_cpu_argb(&frame(1280, 720, 20), 1280 * 4, 1, 25, false)
                    .expect("encode 720p");
                assert_no_reorder(
                    &key,
                    &format!("NVENC fullcolor {fullcolor} 720p after the resize"),
                );
            }
        }
    }

    /// On a real GPU, a resize the driver refuses ends in the rebuild its callers already do:
    /// the ladder the session came from opens a new one at the new size, declaring the buffer
    /// its own level admits. Forced by asking to raise the buffer a 1080p session declared,
    /// which the driver refuses. Ignored by default.
    #[test]
    #[ignore]
    fn gpu_a_refused_resize_is_rebuilt() {
        use crate::encoders::sps::h264_max_num_ref_frames;
        use crate::encoders::{FrameEncoder, FrameSource, select_frame_encoder};
        let mut s = settings(1920, 1080, 60.0);
        let mut enc = host_session(&s).expect("NVENC init");
        let key = enc
            .encode_cpu_argb(&frame(1920, 1080, 10), 1920 * 4, 0, 25, true)
            .expect("encode 1080p");
        assert_eq!(
            h264_max_num_ref_frames(&key[VIDEO_HEADER_LEN..]),
            Some(4),
            "1080p at level 4.2 admits four"
        );
        enc.dpb = REFERENCE_FRAMES;
        s.width = 1280;
        s.height = 720;
        let prior = Some(FrameEncoder::Nvenc(enc));
        let Some(FrameEncoder::Nvenc(mut enc)) =
            select_frame_encoder(&mut s, FrameSource::Host { rgba: false }, prior, "test")
        else {
            panic!("the ladder rebuilt no NVENC session");
        };
        assert_eq!(
            enc.references.as_ref().map_or(0, ReferenceWindow::next_pts),
            0,
            "a new session, not the refused one"
        );
        let key = enc
            .encode_cpu_argb(&frame(1280, 720, 20), 1280 * 4, 1, 25, false)
            .expect("encode 720p");
        assert_eq!(key[1] & 0x0f, FRAME_KEY);
        assert_eq!(wire_dims(&key), (1280, 720));
        assert_eq!(
            h264_max_num_ref_frames(&key[VIDEO_HEADER_LEN..]),
            Some(REFERENCE_FRAMES),
            "720p declares eight"
        );
    }

    /// Test helper: `value` placed so it ends where a page the process cannot touch begins, so a
    /// driver reading or writing past it faults. The mapping lives as long as the process.
    fn guarded<T>(value: T) -> &'static mut T {
        unsafe {
            let page = libc::sysconf(libc::_SC_PAGESIZE) as usize;
            let map = libc::mmap(
                ptr::null_mut(),
                2 * page,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            );
            assert_ne!(map, libc::MAP_FAILED);
            assert_eq!(
                libc::mprotect(map.cast::<u8>().add(page).cast(), page, libc::PROT_NONE),
                0
            );
            let at = map
                .cast::<u8>()
                .add(page - std::mem::size_of::<T>())
                .cast::<T>();
            at.write(value);
            &mut *at
        }
    }

    /// On a real GPU, at every API version a driver may be negotiated at, the driver stays inside
    /// the structs it is handed: each is placed against a page the process cannot touch, where a
    /// read or write past it faults, and every version runs in a process of its own, pinned there
    /// with `PIXELFLUX_NVENC_MAX_API`. Ignored by default.
    #[test]
    #[ignore]
    fn gpu_the_driver_stays_inside_the_structs_it_is_handed() {
        const PINNED: &str = "PIXELFLUX_TEST_NVENC_PINNED";
        if std::env::var_os(PINNED).is_none() {
            let name = format!(
                "{}::gpu_the_driver_stays_inside_the_structs_it_is_handed",
                module_path!().split_once("::").unwrap().1
            );
            for api in ["10.0", "11.0", "11.1", "12.0", "12.1", "13.0"] {
                let out = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        name.as_str(),
                        "--exact",
                        "--ignored",
                        "--nocapture",
                        "--test-threads=1",
                    ])
                    .env("PIXELFLUX_NVENC_MAX_API", api)
                    .env(PINNED, "1")
                    .output()
                    .expect("the test binary runs");
                let stdout = String::from_utf8_lossy(&out.stdout);
                let report = stdout
                    .lines()
                    .find(|l| l.contains("stayed inside"))
                    .unwrap_or_default();
                println!("{report}");
                assert!(
                    out.status.success() && !report.is_empty(),
                    "API {api}: {}\n{stdout}",
                    out.status
                );
            }
            return;
        }
        let mut enc = host_session(&settings(1280, 720, 60.0)).expect("NVENC init");
        unsafe {
            let funcs = enc.nvenc_funcs;
            let _ = (enc.cuda.cuCtxPushCurrent_v2)(enc.cuda_context);
            let mut open = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
                version: sv(NvStruct::OpenSessionExParams),
                deviceType: NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_CUDA,
                device: enc.cuda_context as *mut c_void,
                apiVersion: neg_api(),
                ..Default::default()
            };
            let mut session = ptr::null_mut();
            assert_eq!(
                (funcs.nvEncOpenEncodeSessionEx.unwrap())(&mut open, &mut session),
                NVENCSTATUS::NV_ENC_SUCCESS
            );
            enc.init_params.encodeConfig = &mut enc.encode_config;
            let init = guarded(Negotiated::new(enc.init_params));
            let status = (funcs.nvEncInitializeEncoder.unwrap())(session, &mut init.value);
            (funcs.nvEncDestroyEncoder.unwrap())(session);
            assert_eq!(status, NVENCSTATUS::NV_ENC_SUCCESS, "initialize");
            let reconfigure = guarded(reconfigure_params(
                enc.init_params,
                nvenc_cur_ver(),
                true,
                true,
            ));
            let status = (funcs.nvEncReconfigureEncoder.unwrap())(
                enc.encoder_session,
                &mut reconfigure.value,
            );
            assert_eq!(status, NVENCSTATUS::NV_ENC_SUCCESS, "reconfigure");
            let output = enc.bitstream_buffers[0];
            let mut pic = NV_ENC_PIC_PARAMS {
                version: sv(NvStruct::PicParams),
                inputWidth: enc.width,
                inputHeight: enc.height,
                inputBuffer: enc.mapped_input_buffer,
                outputBitstream: output,
                bufferFmt: enc.input_format,
                pictureStruct: NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME,
                ..Default::default()
            };
            assert_eq!(
                (funcs.nvEncEncodePicture.unwrap())(enc.encoder_session, &mut pic),
                NVENCSTATUS::NV_ENC_SUCCESS
            );
            let lock = guarded(Negotiated::new(NV_ENC_LOCK_BITSTREAM {
                version: sv(NvStruct::LockBitstream),
                outputBitstream: output,
                ..Default::default()
            }));
            let status = (funcs.nvEncLockBitstream.unwrap())(enc.encoder_session, &mut lock.value);
            assert_eq!(status, NVENCSTATUS::NV_ENC_SUCCESS, "lock");
            assert!(
                lock.value.bitstreamSizeInBytes > 0,
                "the locked bitstream carries the picture"
            );
            (funcs.nvEncUnlockBitstream.unwrap())(enc.encoder_session, output);
            (enc.cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
        }
        let (maj, min) = nvenc_cur_ver();
        println!("API {maj}.{min}: the driver stayed inside every struct");
    }

    /// A source a capture keeps and repaints, as its shm segment is, is page-locked once and
    /// read as it is at each upload, and after `release_pinned_hosts`, as a reshape calls it, a
    /// new one is: each decoded frame shows the picture just painted, not an earlier one.
    /// Ignored by default.
    #[test]
    #[ignore]
    fn gpu_pinned_uploads_read_the_source_as_it_is_now() {
        use crate::webcam::decode::VideoDecoder;
        let (w, h) = (1280usize, 720usize);
        let mut enc = NvencEncoder::new(&settings(w as i32, h as i32, 60.0), ptr::null())
            .expect("NVENC init");
        assert!(enc.pin_uploads, "the production default pins");
        let mut dec = VideoDecoder::new(Codec::H264).expect("decoder");
        for (source, frames) in [(0, 0..4usize), (1, 4..8usize)] {
            if source == 1 {
                enc.release_pinned_hosts();
            }
            let mut shm = vec![0u8; w * h * 4];
            for i in frames {
                shm.copy_from_slice(&moving_frame(w, h, i));
                let pkt = enc
                    .encode_cpu_packed(&shm, w * 4, false, i as u64, 25, i == 0)
                    .expect("encode");
                let psnr = luma_psnr(&mut dec, &pkt, &moving_frame(w, h, i), w, h);
                assert!(
                    psnr > 30.0,
                    "source {source}, frame {i}: {psnr:.1} dB against the picture just painted"
                );
            }
        }
    }

    /// On a real GPU whose driver caps the NVENC sessions a device runs at once, as it does on
    /// consumer boards, the probe of a device with every session taken answers `SESSIONS_TAKEN`,
    /// and once one frees it lists the codecs again. A device that takes 65 sessions at once has
    /// no cap to reach and says so. The sessions are closed before anything is asserted, so a
    /// failure leaves none held against the cap for the tests after it. Ignored by default.
    /// A capture restarted while a game holds the GPU's memory keeps its place: once a session
    /// has opened, the device's primary CUDA context outlives it (`keep_primary_context`), so a
    /// session reopened after another context has taken all but 48 MiB still opens and encodes,
    /// where a context created afresh needs about 106 MiB.
    #[test]
    #[ignore]
    fn gpu_a_session_reopened_under_a_full_gpu_keeps_its_context() {
        type Ctx = *mut c_void;
        let first = host_session(&settings(256, 128, 60.0)).expect("NVENC init");
        let device = first.cuda_device;
        drop(first);
        unsafe {
            let lib = Library::new("libcuda.so.1").expect("libcuda");
            let state: Symbol<unsafe extern "C" fn(CUdevice, *mut u32, *mut i32) -> CUresult> =
                lib.get(b"cuDevicePrimaryCtxGetState\0").unwrap();
            let create: Symbol<unsafe extern "C" fn(*mut Ctx, u32, CUdevice) -> CUresult> =
                lib.get(b"cuCtxCreate_v2\0").unwrap();
            let destroy: Symbol<unsafe extern "C" fn(Ctx) -> CUresult> =
                lib.get(b"cuCtxDestroy_v2\0").unwrap();
            let info: Symbol<unsafe extern "C" fn(*mut usize, *mut usize) -> CUresult> =
                lib.get(b"cuMemGetInfo_v2\0").unwrap();
            let alloc: Symbol<unsafe extern "C" fn(*mut u64, usize) -> CUresult> =
                lib.get(b"cuMemAlloc_v2\0").unwrap();
            let (mut flags, mut active) = (0u32, 0i32);
            assert_eq!(
                state(device, &mut flags, &mut active),
                CUresult::CUDA_SUCCESS
            );

            let mut game: Ctx = ptr::null_mut();
            assert_eq!(create(&mut game, 0, device), CUresult::CUDA_SUCCESS);
            let (mut free, mut total) = (0usize, 0usize);
            let mut chunk = 256usize << 20;
            let mut taken = 0usize;
            while chunk >= 1 << 20 {
                let mut ptr = 0u64;
                info(&mut free, &mut total);
                if free < (48 << 20) + chunk || alloc(&mut ptr, chunk) != CUresult::CUDA_SUCCESS {
                    chunk /= 2;
                } else {
                    taken += chunk;
                }
            }
            info(&mut free, &mut total);
            println!(
                "took {} MiB; {} MiB of {} left",
                taken >> 20,
                free >> 20,
                total >> 20
            );
            let reopened = host_session(&settings(256, 128, 60.0)).map(|mut enc| {
                enc.encode_cpu_argb(&vec![0x40u8; 256 * 128 * 4], 256 * 4, 0, 25, true)
                    .map(|out| out.len())
            });
            destroy(game);
            println!(
                "primary context active after the first session: {active}; reopened: {reopened:?}"
            );
            assert_eq!(
                active, 1,
                "the primary context outlived the session that opened it"
            );
            let bytes = reopened.expect("the session reopens").expect("and encodes");
            assert!(bytes > 0);
        }
    }

    #[test]
    #[ignore]
    fn gpu_a_device_out_of_sessions_says_so() {
        let enc = host_session(&settings(256, 128, 60.0)).expect("NVENC init");
        let mut sessions = Vec::new();
        let refused = unsafe {
            loop {
                let mut open = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
                    version: sv(NvStruct::OpenSessionExParams),
                    deviceType: NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_CUDA,
                    device: enc.cuda_context as *mut c_void,
                    apiVersion: neg_api(),
                    ..Default::default()
                };
                let mut session = ptr::null_mut();
                match (enc.nvenc_funcs.nvEncOpenEncodeSessionEx.unwrap())(&mut open, &mut session) {
                    NVENCSTATUS::NV_ENC_SUCCESS => sessions.push(session),
                    status => break Some(status),
                }
                if sessions.len() == 64 {
                    break None;
                }
            }
        };
        let taken = probe_codecs(0);
        let opened = sessions.len();
        for session in sessions.drain(..) {
            unsafe { (enc.nvenc_funcs.nvEncDestroyEncoder.unwrap())(session) };
        }
        let Some(status) = refused else {
            println!("this device took 65 sessions at once: no cap to reach");
            return;
        };
        assert_eq!(
            session_refusal(status),
            SESSIONS_TAKEN,
            "session {} refused: {status:?}",
            opened + 2
        );
        assert_eq!(
            taken,
            Err(SESSIONS_TAKEN.to_string()),
            "every session taken"
        );
        assert!(
            probe_codecs(0).is_ok_and(|codecs| !codecs.is_empty()),
            "a freed session is listed again"
        );
    }

    /// On a real GPU, a live frame-rate drop keeps the level the decoded picture buffer needs: a
    /// 1080p session opened at 120 fps declares the eight frames its level admits there, and the
    /// drop to 60 fps, whose own level admits fewer, is taken at that level, so a CBR session
    /// spends about twice the bits on each frame. Ignored by default.
    #[test]
    #[ignore]
    fn gpu_frame_rate_drop_keeps_the_level_the_buffer_needs() {
        for codec in [Codec::H264, Codec::H265] {
            let mut s = settings(1920, 1080, 120.0);
            s.codec = codec;
            s.video_cbr_mode = true;
            s.video_bitrate_kbps = 8000;
            let Some(mut enc) = codec_session(&s) else {
                continue;
            };
            let level = enc.declared_level();
            let frames: Vec<Vec<u8>> = (0..16).map(|i| moving_frame(1920, 1080, i)).collect();
            let kbit = |enc: &mut NvencEncoder, range: std::ops::Range<u64>| {
                let n = (range.end - range.start) as f64;
                let bytes: usize = range
                    .map(|i| {
                        enc.encode_cpu_argb(&frames[i as usize % 16], 1920 * 4, i, 25, i == 0)
                            .expect("encode")
                            .len()
                            - VIDEO_HEADER_LEN
                    })
                    .sum();
                bytes as f64 * 8.0 / n / 1000.0
            };
            kbit(&mut enc, 0..20);
            let at120 = kbit(&mut enc, 20..80);
            s.target_fps = 60.0;
            assert!(
                enc.reconfigure_rate(&s),
                "{codec:?}: the driver refused 60 fps"
            );
            assert_eq!(
                enc.declared_level(),
                level,
                "{codec:?}: the level the buffer needs stays"
            );
            let at60 = kbit(&mut enc, 80..200);
            println!("{codec:?}: {at120:.1} kbit a frame at 120 fps, {at60:.1} at 60");
            assert!(
                at60 > 1.4 * at120,
                "{codec:?}: halving the frame rate left {at60:.1} kbit a frame against {at120:.1}"
            );
        }
    }

    /// On a real GPU, a session of every codec the device carries is opened at the capture's
    /// rate as the fraction it names, a live change of rate reaches the driver the same way, and
    /// an H.264 stream declares the rate in its timing. Ignored by default.
    #[test]
    #[ignore]
    fn gpu_frame_rate_reaches_the_driver_as_its_fraction() {
        let f = frame(640, 360, 30);
        for codec in [Codec::H264, Codec::H265, Codec::Av1] {
            for (num, den) in [(60000u32, 1001u32), (120000, 1001), (144000, 1001), (60, 1)] {
                let fps = num as f64 / den as f64;
                let mut s = settings(640, 360, fps);
                s.codec = codec;
                s.video_cbr_mode = true;
                s.video_bitrate_kbps = 8000;
                let mut enc = match host_session(&s) {
                    Ok(enc) => enc,
                    Err(e) if e.contains("engine") => {
                        println!("{codec:?}: {e}");
                        break;
                    }
                    Err(e) => panic!("{codec:?} at {num}/{den}: {e}"),
                };
                assert_eq!(
                    (enc.init_params.frameRateNum, enc.init_params.frameRateDen),
                    (num, den),
                    "{codec:?}"
                );
                let pkt = enc
                    .encode_cpu_argb(&f, 640 * 4, 0, 25, true)
                    .expect("encode");
                if codec == Codec::H264 {
                    let (tick, scale) = crate::encoders::sps::h264_timing(&pkt[VIDEO_HEADER_LEN..])
                        .expect("SPS timing");
                    assert_eq!(
                        scale as u64 * den as u64,
                        2 * num as u64 * tick as u64,
                        "the SPS declares {scale}/2x{tick}, not {num}/{den}"
                    );
                }
                s.target_fps = 30.0;
                assert!(
                    enc.reconfigure_rate(&s),
                    "{codec:?}: the driver refused 30 fps"
                );
                assert_eq!(
                    (enc.init_params.frameRateNum, enc.init_params.frameRateDen),
                    (30, 1)
                );
                s.target_fps = fps;
                assert!(
                    enc.reconfigure_rate(&s),
                    "{codec:?}: the driver refused {num}/{den}"
                );
                assert_eq!(
                    (enc.init_params.frameRateNum, enc.init_params.frameRateDen),
                    (num, den),
                    "{codec:?}"
                );
                enc.encode_cpu_argb(&f, 640 * 4, 1, 25, false)
                    .expect("encode after the change");
            }
        }
    }

    /// On a real GPU, a live frame-rate rise past the declared level's macroblock rate raises the
    /// level at a key frame, whose sequence header is where a decoder learns it: a 1080p session
    /// opened at 60 fps declares level 4.2, and 120 fps takes 5.1. Ignored by default.
    #[test]
    #[ignore]
    fn gpu_frame_rate_rise_raises_the_level_at_a_key_frame() {
        let mut s = settings(1920, 1080, 60.0);
        let mut enc = host_session(&s).expect("NVENC init");
        let f = frame(1920, 1080, 30);
        for i in 0..3u64 {
            enc.encode_cpu_argb(&f, 1920 * 4, i, 25, i == 0)
                .expect("encode");
        }
        assert_eq!(enc.declared_level(), 42);
        s.target_fps = 120.0;
        assert!(enc.reconfigure_rate(&s), "the driver refused 120 fps");
        let pkt = enc
            .encode_cpu_argb(&f, 1920 * 4, 3, 25, false)
            .expect("encode");
        assert_eq!(
            pkt[1] & 0x0f,
            FRAME_KEY,
            "the raised level reaches the stream at a key frame"
        );
        let sps = crate::encoders::codec::annexb_nals(&pkt[VIDEO_HEADER_LEN..])
            .find(|n| n[0] & 0x1f == 7)
            .expect("an SPS");
        assert_eq!(sps[3], 51, "level_idc");
    }

    /// On a real GPU, a live CBR rate change moves the VBV initial delay with the buffer: after
    /// the bitrate is lowered the delay equals the new, smaller buffer, the driver takes the
    /// reconfigure, and a steady desktop-like sequence lands at the new rate. What the driver
    /// makes of a delay left above the buffer is printed rather than asserted. Ignored by default.
    /// A capped constant quantizer under a peak its picture never reaches codes every frame as
    /// the uncapped session does, byte for byte: it never leaves the constant quantizer. Ignored
    /// by default.
    #[test]
    #[ignore]
    fn gpu_a_capped_constant_quantizer_codes_as_uncapped_under_a_peak_it_does_not_reach() {
        let (w, h) = (640usize, 360usize);
        let plain = settings(w as i32, h as i32, 60.0);
        let capped = CaptureSettings {
            video_bitrate_kbps: 200_000,
            video_vbv_multiplier: crate::encoders::CAPPED_VBV_FRAMES,
            ..plain.clone()
        };
        let mut a = host_session(&plain).expect("NVENC init");
        let mut b = host_session(&capped).expect("NVENC init");
        assert!(a.peak.is_none() && b.peak.is_some());
        for i in 0..60u64 {
            let f = moving_frame(w, h, i as usize);
            let one = a.encode_cpu_argb(&f, w * 4, i, 25, i == 0).expect("encode");
            let two = b.encode_cpu_argb(&f, w * 4, i, 25, i == 0).expect("encode");
            assert!(
                one == two,
                "frame {i}: {} bytes uncapped, {} capped",
                one.len(),
                two.len()
            );
        }
        assert_eq!(
            b.encode_config.rcParams.rateControlMode,
            NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CONSTQP
        );
    }

    /// A capped constant quantizer whose content overflows its peak binds to it without a key
    /// frame and codes within it, never finer than its quantizer, and once the peak rises past
    /// what the content takes goes back to the constant quantizer, again without one. Ignored
    /// by default.
    #[test]
    #[ignore]
    fn gpu_a_capped_constant_quantizer_binds_and_releases_without_a_key_frame() {
        use crate::encoders::codec::{FRAME_KEY, parse_video_type};
        let (w, h) = (640usize, 360usize);
        let mut s = CaptureSettings {
            video_bitrate_kbps: 200_000,
            video_vbv_multiplier: crate::encoders::CAPPED_VBV_FRAMES,
            ..settings(w as i32, h as i32, 60.0)
        };
        let mut enc = host_session(&s).expect("NVENC init");
        let floor = enc.current_qp;
        let mut sizes = Vec::new();
        let code = |enc: &mut NvencEncoder, i: u64, crf: u32| {
            let out = enc
                .encode_cpu_argb(
                    &crate::encoders::scrolled_text(w, h, i as usize),
                    w * 4,
                    i,
                    crf,
                    i == 0,
                )
                .expect("encode");
            assert!(
                i == 0 || parse_video_type(out[1]).map(|(_, k)| k) != Some(FRAME_KEY),
                "frame {i} is a key frame"
            );
            (out.len() - VIDEO_HEADER_LEN) * 8
        };
        for i in 0..30 {
            sizes.push(code(&mut enc, i, 25));
        }
        let kbps = |f: &[usize]| f.iter().sum::<usize>() as f64 * 60.0 / f.len() as f64 / 1000.0;
        let free = kbps(&sizes[1..]);
        s.video_bitrate_kbps = (free / 3.0) as i32;
        enc.reconfigure_rate(&s);
        let mut bound = Vec::new();
        for i in 30..150 {
            bound.push(code(&mut enc, i, 25));
            if i >= 40 {
                assert!(
                    enc.last_avg_qp >= floor,
                    "frame {i} coded finer than the quantizer"
                );
            }
        }
        assert_eq!(
            enc.encode_config.rcParams.rateControlMode,
            NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR,
            "content over the peak binds the session"
        );
        let held = kbps(&bound[30..]);
        println!(
            "free {free:.0} kbps, held {held:.0} kbps under a {} kbps peak",
            s.video_bitrate_kbps
        );
        assert!(
            held <= 1.1 * s.video_bitrate_kbps as f64,
            "the session ran at {held:.0} kbps under a {} kbps peak",
            s.video_bitrate_kbps
        );
        s.video_bitrate_kbps = 200_000;
        enc.reconfigure_rate(&s);
        for i in 150..150 + PeakBucket::RELEASE_FRAMES as u64 + 10 {
            code(&mut enc, i, 25);
        }
        assert_eq!(
            enc.encode_config.rcParams.rateControlMode,
            NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CONSTQP,
            "a peak above what the content takes releases it"
        );
        assert_eq!(enc.last_avg_qp, floor);
        // Released, it codes at the quantizer the session asks for, as an uncapped one does.
        let paint = enc.codec.hardware_quantizer(Hardware::Nvenc, 18);
        for i in 190..200 {
            code(&mut enc, i, 18);
        }
        assert_eq!(
            enc.last_avg_qp, paint,
            "a released session codes at its paint-over quantizer"
        );
        for i in 200..210 {
            code(&mut enc, i, 25);
        }
        assert_eq!(enc.last_avg_qp, floor);
    }

    /// A capped constant quantizer bound to its peak stays bound while frames refining every
    /// block through the QP map 25 steps finer are coded: the constant rate holds their mean
    /// quantizer over the floor, so none counts toward the release. Ignored by default.
    #[test]
    #[ignore]
    fn gpu_a_bound_session_refining_blocks_is_not_released_by_their_deltas() {
        let (w, h) = (640usize, 360usize);
        let mut s = CaptureSettings {
            video_bitrate_kbps: 200_000,
            video_vbv_multiplier: crate::encoders::CAPPED_VBV_FRAMES,
            ..settings(w as i32, h as i32, 60.0)
        };
        let mut enc = host_session(&s).expect("NVENC init");
        let mut ledger = crate::ledger::BlockLedger::new(w as i32, h as i32);
        ledger.frame([]);
        let plan = ledger.plan(0, usize::MAX, usize::MAX).expect("every block");
        let mut sizes = Vec::new();
        let code = |enc: &mut NvencEncoder, i: u64, refine: bool| {
            if refine {
                enc.refine_blocks(&crate::ledger::Refinement {
                    plan: plan.clone(),
                    target: 0,
                    from: 25,
                    reach: u32::MAX,
                });
            }
            let out = enc
                .encode_cpu_argb(
                    &crate::encoders::scrolled_text(w, h, i as usize),
                    w * 4,
                    i,
                    25,
                    i == 0,
                )
                .expect("encode");
            out.len() * 8
        };
        for i in 0..30 {
            sizes.push(code(&mut enc, i, false));
        }
        let free = sizes[1..].iter().sum::<usize>() as f64 * 60.0 / 29.0 / 1000.0;
        s.video_bitrate_kbps = (free / 3.0) as i32;
        enc.reconfigure_rate(&s);
        let (mut released, mut counted) = (0, 0);
        for i in 30..200 {
            code(&mut enc, i, i >= 60);
            if i >= 60 {
                released += usize::from(!enc.peak_bound());
                counted = counted.max(enc.peak.map_or(0, |p| p.floor_run()));
            }
        }
        println!(
            "{released} of 140 refining frames coded released, {counted} counted at the floor in a \
             row, free {free:.0} kbps"
        );
        assert_eq!(
            released, 0,
            "the session let go of a peak its content overflows"
        );
        assert_eq!(counted, 0, "a refining frame counted toward the release");
    }

    #[test]
    #[ignore]
    fn gpu_rate_reconfigure_moves_the_vbv_initial_delay_with_the_buffer() {
        let (w, h) = (1920usize, 1080usize);
        let mut s = settings(w as i32, h as i32, 60.0);
        s.video_cbr_mode = true;
        s.video_bitrate_kbps = 8000;
        let mut enc = host_session(&s).expect("NVENC init");
        let rate = |enc: &NvencEncoder| {
            let rc = enc.encode_config.rcParams;
            (rc.averageBitRate, rc.vbvBufferSize, rc.vbvInitialDelay)
        };
        let vbv8 = cbr_vbv(&s, 8_000_000);
        assert_eq!(rate(&enc), (8_000_000, vbv8, vbv8));
        let frames: Vec<Vec<u8>> = (0..16).map(|i| moving_frame(w, h, i)).collect();
        enc.encode_cpu_argb(&frames[0], w * 4, 0, 25, true)
            .expect("encode");

        s.video_bitrate_kbps = 2000;
        assert!(enc.reconfigure_rate(&s), "the driver takes the lower rate");
        let vbv2 = cbr_vbv(&s, 2_000_000);
        assert_eq!(rate(&enc), (2_000_000, vbv2, vbv2));
        let n = 120u64;
        let mut bytes = 0usize;
        for i in 1..=n {
            let pkt = enc
                .encode_cpu_argb(&frames[i as usize % 16], w * 4, i, 25, false)
                .expect("encode");
            bytes += pkt.len().saturating_sub(VIDEO_HEADER_LEN);
        }
        let kbps = bytes as f64 * 8.0 * 60.0 / n as f64 / 1000.0;
        println!("CBR 2000 kbps after the rate change: {kbps:.0} kbps over {n} steady frames");
        assert!(
            (1500.0..=2500.0).contains(&kbps),
            "the session encodes at the new rate: {kbps:.0} kbps"
        );

        enc.encode_config.rcParams.vbvInitialDelay = vbv2 * 4;
        let (status, detail) = reconfigure_raw(&mut enc);
        println!("a VBV initial delay of four buffers: {status:?} ({detail})");
        enc.encode_config.rcParams.vbvInitialDelay = vbv2;
        assert_eq!(reconfigure_raw(&mut enc).0, NVENCSTATUS::NV_ENC_SUCCESS);
        enc.encode_cpu_argb(&frames[7], w * 4, n + 1, 25, false)
            .expect("the session still encodes");
    }

    /// The slice count and the HEVC tier are properties of the bitstream a client decodes, so
    /// they are read back out of it on a real GPU: four VCL NAL units per H.264 frame and one
    /// per HEVC frame at two geometries, and an HEVC SPS carrying the High tier flag beside the
    /// pinned level. Ignored by default.
    #[test]
    #[ignore]
    fn gpu_frames_carry_their_slices_and_hevc_declares_high_tier() {
        use crate::encoders::codec::annexb_nals;
        for (w, h) in [(1280usize, 720usize), (640, 480)] {
            for codec in [Codec::H264, Codec::H265] {
                let mut s = settings(w as i32, h as i32, 60.0);
                s.codec = codec;
                let Some(mut enc) = codec_session(&s) else {
                    continue;
                };
                for i in 0..4u64 {
                    let pkt = enc
                        .encode_cpu_argb(&frame(w, h, 20 + i as u8), w * 4, i, 25, i == 0)
                        .expect("encode");
                    let au = &pkt[VIDEO_HEADER_LEN..];
                    let vcl = annexb_nals(au)
                        .filter(|nal| match codec {
                            Codec::H265 => (nal[0] >> 1) & 0x3f < 32,
                            _ => matches!(nal[0] & 0x1f, 1 | 5),
                        })
                        .count();
                    let slices = if codec == Codec::H265 {
                        1
                    } else {
                        SLICES_PER_FRAME as usize
                    };
                    assert_eq!(vcl, slices, "{codec:?} {w}x{h} frame {i} slices");
                    if codec == Codec::H264 && i == 0 {
                        // After the one-byte NAL header the SPS RBSP carries profile_idc, the
                        // constraint-flag byte, then level_idc, which must be the current
                        // geometry's level rather than the 4K-headroom one so a decoder gating
                        // on it accepts the picture.
                        let sps = annexb_nals(au)
                            .find(|nal| nal[0] & 0x1f == 7)
                            .map(rbsp)
                            .expect("an SPS on the key frame");
                        assert_eq!(
                            sps[3] as u32,
                            nvenc_level(Codec::H264, w as u32, h as u32, 60, 0, true),
                            "level_idc at {w}x{h}"
                        );
                    }
                    if codec == Codec::H265 && i == 0 {
                        let sps = annexb_nals(au)
                            .find(|nal| (nal[0] >> 1) & 0x3f == 33)
                            .map(rbsp)
                            .expect("an SPS on the key frame");
                        // After the two-byte NAL header and the byte holding the VPS id, the
                        // sub-layer count, and the nesting flag, profile_tier_level opens with
                        // general_profile_space (2), general_tier_flag (1), and
                        // general_profile_idc (5); general_level_idc follows the 32
                        // compatibility flags and the 48 constraint bits.
                        assert_eq!((sps[3] >> 5) & 1, 1, "general_tier_flag at {w}x{h}");
                        assert_eq!(
                            sps[14] as u32,
                            nvenc_level(Codec::H265, w as u32, h as u32, 60, 0, true),
                            "general_level_idc at {w}x{h}"
                        );
                    }
                }
            }
        }
    }

    /// The first coding tree block of each slice of an HEVC access unit, read out of the slice
    /// segment headers (the PPS has no dependent slice segments, which NVENC never enables), and
    /// whether the access unit's PPS, where it carries one, filters across slice boundaries.
    fn hevc_slice_starts(au: &[u8], ctbs: usize) -> (Vec<u32>, Option<bool>) {
        use crate::encoders::codec::annexb_nals;
        struct Bits<'a>(&'a [u8], usize);
        impl Bits<'_> {
            fn u(&mut self, n: u32) -> u32 {
                (0..n).fold(0, |acc, _| {
                    let bit = (self.0[self.1 / 8] >> (7 - self.1 % 8)) & 1;
                    self.1 += 1;
                    (acc << 1) | bit as u32
                })
            }
            fn ue(&mut self) -> u32 {
                let mut zeros = 0;
                while self.u(1) == 0 {
                    zeros += 1;
                }
                (1 << zeros) - 1 + self.u(zeros)
            }
        }
        let address_bits = usize::BITS - (ctbs - 1).leading_zeros();
        let (mut starts, mut across) = (Vec::new(), None);
        for nal in annexb_nals(au) {
            let kind = (nal[0] >> 1) & 0x3f;
            let body = rbsp(nal);
            let mut b = Bits(&body[2..], 0);
            if kind == 34 {
                b.ue();
                b.ue();
                b.u(7);
                b.ue();
                b.ue();
                let _qp = b.ue();
                b.u(2);
                if b.u(1) == 1 {
                    b.ue();
                }
                b.ue();
                b.ue();
                b.u(4);
                let tiles = b.u(1) == 1;
                b.u(1);
                assert!(!tiles, "NVENC's HEVC codes no tiles");
                across = Some(b.u(1) == 1);
            } else if kind < 32 {
                let first = b.u(1) == 1;
                if (16..=23).contains(&kind) {
                    b.u(1);
                }
                b.ue();
                starts.push(if first { 0 } else { b.u(address_bits) });
            }
        }
        (starts, across)
    }

    /// On a real GPU: an HEVC band frame starts a slice at its band's first coding tree block,
    /// across which the session never filters, so the rows the bands before refined keep their
    /// pixels; a band starting within the first `BAND_SLICES_MAX`th keeps the frame's one slice,
    /// and the frame after it is one slice again. Every frame decodes. Ignored by default.
    #[test]
    #[ignore]
    fn gpu_hevc_band_frames_start_a_slice_at_the_band() {
        use crate::webcam::decode::{Decoder as _, VideoDecoder};
        let (w, h) = (1920usize, 1080usize);
        let ctbs = w.div_ceil(32) * h.div_ceil(32);
        let mut s = settings(w as i32, h as i32, 60.0);
        s.codec = Codec::H265;
        s.omit_stripe_headers = true;
        let Some(mut enc) = codec_session(&s) else {
            return;
        };
        let still = frame(w, h, 40);
        let mut frames = Vec::new();
        let mut encode = |enc: &mut NvencEncoder, i: u64| {
            let pkt = enc
                .encode_cpu_argb(&still, w * 4, i, 25, i == 0)
                .expect("encode");
            frames.push(pkt.clone());
            hevc_slice_starts(&pkt, ctbs)
        };
        let (starts, across) = encode(&mut enc, 0);
        assert_eq!(starts, [0], "a key frame is one slice");
        assert_eq!(
            across,
            Some(false),
            "the PPS filters across no slice boundary"
        );
        let mut i = 1u64;
        for share in [0.5, 0.25, 0.75, 0.13, 0.05] {
            assert_eq!(
                encode(&mut enc, i).0,
                [0],
                "frame {i} before a band is one slice"
            );
            i += 1;
            enc.hold_quantizer(10, Some((share, share + 0.02)));
            let (starts, _) = encode(&mut enc, i);
            i += 1;
            let first = (share * ctbs as f64).floor() as usize;
            if ctbs.div_ceil(first) <= BAND_SLICES_MAX {
                assert!(
                    starts.contains(&(first as u32)) && starts.len() <= BAND_SLICES_MAX,
                    "a band from {first} starts a slice: {starts:?}"
                );
            } else {
                assert_eq!(starts, [0], "a band from {first} keeps the frame's slice");
            }
        }
        assert_eq!(
            encode(&mut enc, i).0,
            [0],
            "the frame after a band is one slice"
        );
        let mut decoder = VideoDecoder::new(Codec::H265).unwrap();
        for (k, f) in frames.iter().enumerate() {
            assert!(decoder.decode(f).expect("decode"), "frame {k}");
        }
    }

    /// On a real GPU: what `SLICES_PER_FRAME` slices cost against one at a fixed quantizer,
    /// H.264 and HEVC at 1080p on the gradient frames the other benches use. Prints the rates and
    /// the difference; ignored by default.
    #[test]
    #[ignore]
    fn gpu_bench_slices() {
        let (w, h) = (1920usize, 1080usize);
        let n: usize = std::env::var("NVENC_BENCH_FRAMES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(240);
        let frames: Vec<Vec<u8>> = (0..8u8).map(|k| frame(w, h, 10 + 30 * k)).collect();
        for codec in [Codec::H264, Codec::H265] {
            let mut s = settings(w as i32, h as i32, 60.0);
            s.codec = codec;
            let mut sizes = Vec::new();
            for slices in [1u32, SLICES_PER_FRAME] {
                let tuning = NvencTuning {
                    slices: Some(slices),
                    ..NvencTuning::default()
                };
                let mut enc = match NvencEncoder::new_tuned(&s, ptr::null(), tuning) {
                    Ok(enc) => enc,
                    Err(e) => {
                        println!("{codec:?}: {e}");
                        continue;
                    }
                };
                enc.encode_cpu_packed(&frames[0], w * 4, false, 0, 25, true)
                    .expect("warm-up");
                let mut bytes = 0usize;
                per_frame(&format!("{codec:?} {slices} slice(s)"), n, |i| {
                    bytes += enc
                        .encode_cpu_packed(&frames[i % 8], w * 4, false, 1 + i as u64, 25, false)
                        .expect("encode")
                        .len();
                });
                println!("    {} kbps", bytes * 8 * 60 / n / 1000);
                sizes.push(bytes);
            }
            if let [one, many] = sizes[..] {
                println!(
                    "{codec:?}: {SLICES_PER_FRAME} slices cost {:+.2}% bitrate at a fixed quantizer",
                    (many as f64 / one as f64 - 1.0) * 100.0
                );
            }
        }
    }

    /// On a real GPU: how closely a CBR session holds its target under each VBV policy choice,
    /// for H.264 and HEVC at 1080p, on three kinds of content: the alternating gradient frames
    /// the other benches use, where every frame is a scene cut; a steady desktop-like sequence
    /// with one moving block; and that sequence entered through one cut. Each row varies the VBV
    /// (one frame or one and a half), the initial delay (a full buffer or the driver's default),
    /// and the slice count (`SLICES_PER_FRAME` or one), and prints the achieved rate, the
    /// largest and smallest frame, the luma PSNR of the decoded picture against the source (mean
    /// and worst frame), and the wall time per frame. `NVENC_BENCH_KBPS` and `NVENC_BENCH_FPS`
    /// move the target from 8000 kbit/s at 60 fps. Ignored by default.
    #[test]
    #[ignore]
    fn gpu_bench_cbr_policy() {
        let (w, h) = (1920usize, 1080usize);
        let env = |name: &str, default: usize| {
            std::env::var(name)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(default)
        };
        let n = env("NVENC_BENCH_FRAMES", 120);
        let kbps = env("NVENC_BENCH_KBPS", 8000);
        let fps = env("NVENC_BENCH_FPS", 60);
        let cuts: Vec<Vec<u8>> = (0..8u8).map(|k| frame(w, h, 10 + 30 * k)).collect();
        let steady: Vec<Vec<u8>> = (0..16).map(|i| moving_frame(w, h, i)).collect();
        let other = frame(w, h, 200);
        let contents: Vec<(&str, Vec<&Vec<u8>>)> = vec![
            (
                "scene cuts",
                std::iter::once(&cuts[0])
                    .chain((0..n).map(|i| &cuts[1 + i % 7]))
                    .collect(),
            ),
            (
                "steady",
                std::iter::once(&steady[0])
                    .chain((0..n).map(|i| &steady[1 + i % 15]))
                    .collect(),
            ),
            (
                "one cut",
                std::iter::once(&other)
                    .chain((0..n).map(|i| &steady[i % 16]))
                    .collect(),
            ),
        ];
        println!("CBR {kbps} kbit/s at {fps} fps, {n} frames per row");
        for codec in [Codec::H264, Codec::H265] {
            for (content, seq) in &contents {
                for vbv_frames in [1.0f64, 1.5] {
                    for full_delay in [true, false] {
                        for slices in [SLICES_PER_FRAME, 1] {
                            let mut s = settings(w as i32, h as i32, fps as f64);
                            s.codec = codec;
                            s.video_cbr_mode = true;
                            s.video_bitrate_kbps = kbps as i32;
                            s.video_vbv_multiplier = vbv_frames;
                            let tuning = NvencTuning {
                                slices: Some(slices),
                                ..NvencTuning::default()
                            };
                            let mut enc = match NvencEncoder::new_tuned(&s, ptr::null(), tuning) {
                                Ok(enc) => enc,
                                Err(e) => {
                                    println!("{codec:?}: {e}");
                                    continue;
                                }
                            };
                            if !full_delay {
                                enc.encode_config.rcParams.vbvInitialDelay = 0;
                                assert_eq!(
                                    reconfigure_raw(&mut enc).0,
                                    NVENCSTATUS::NV_ENC_SUCCESS
                                );
                            }
                            let label = format!(
                                "{codec:?} {content}: VBV {vbv_frames} frame(s), initial delay {}, {slices} slice(s)",
                                if full_delay { "full" } else { "default" }
                            );
                            cbr_row(&label, &mut enc, codec, seq, w, h, fps);
                        }
                    }
                }
            }
        }
    }

    /// On a real GPU: which H.264 rate-control setting lets a CBR session run past a small VBV.
    /// Holds a 1080p H.264 session at the target `NVENC_BENCH_KBPS` and `NVENC_BENCH_FPS` name
    /// on the scene-cut and one-cut sequences at one and one and a half frames of buffer, and
    /// changes one setting per row against the production session: single-pass and
    /// full-resolution two-pass rate control in place of the quarter-resolution first pass,
    /// `strictGOPTarget` off, and a quantizer ceiling of 51. Ignored by default.
    #[test]
    #[ignore]
    fn gpu_bench_cbr_rate_control() {
        let (w, h) = (1920usize, 1080usize);
        let env = |name: &str, default: usize| {
            std::env::var(name)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(default)
        };
        let n = env("NVENC_BENCH_FRAMES", 120);
        let kbps = env("NVENC_BENCH_KBPS", 8000);
        let fps = env("NVENC_BENCH_FPS", 60);
        let cuts: Vec<Vec<u8>> = (0..8u8).map(|k| frame(w, h, 10 + 30 * k)).collect();
        let steady: Vec<Vec<u8>> = (0..16).map(|i| moving_frame(w, h, i)).collect();
        let other = frame(w, h, 200);
        let contents: Vec<(&str, Vec<&Vec<u8>>)> = vec![
            (
                "scene cuts",
                std::iter::once(&cuts[0])
                    .chain((0..n).map(|i| &cuts[1 + i % 7]))
                    .collect(),
            ),
            (
                "one cut",
                std::iter::once(&other)
                    .chain((0..n).map(|i| &steady[i % 16]))
                    .collect(),
            ),
        ];
        let variants = [
            (
                "production",
                NV_ENC_MULTI_PASS::NV_ENC_TWO_PASS_QUARTER_RESOLUTION,
                true,
                0,
            ),
            (
                "single pass",
                NV_ENC_MULTI_PASS::NV_ENC_MULTI_PASS_DISABLED,
                true,
                0,
            ),
            (
                "two-pass full",
                NV_ENC_MULTI_PASS::NV_ENC_TWO_PASS_FULL_RESOLUTION,
                true,
                0,
            ),
            (
                "strictGOPTarget off",
                NV_ENC_MULTI_PASS::NV_ENC_TWO_PASS_QUARTER_RESOLUTION,
                false,
                0,
            ),
            (
                "max QP 51",
                NV_ENC_MULTI_PASS::NV_ENC_TWO_PASS_QUARTER_RESOLUTION,
                true,
                51,
            ),
        ];
        println!("H264 CBR {kbps} kbit/s at {fps} fps, {n} frames per row");
        for (content, seq) in &contents {
            for vbv_frames in [1.0f64, 1.5] {
                for (name, multipass, strict, max_qp) in variants {
                    let mut s = settings(w as i32, h as i32, fps as f64);
                    s.video_cbr_mode = true;
                    s.video_bitrate_kbps = kbps as i32;
                    s.video_vbv_multiplier = vbv_frames;
                    s.video_max_qp = max_qp;
                    let tuning = NvencTuning {
                        multipass,
                        ..NvencTuning::default()
                    };
                    let mut enc = match NvencEncoder::new_tuned(&s, ptr::null(), tuning) {
                        Ok(enc) => enc,
                        Err(e) => {
                            println!("{name}: {e}");
                            continue;
                        }
                    };
                    if !strict {
                        enc.encode_config.rcParams.set_strictGOPTarget(0);
                        assert_eq!(reconfigure_raw(&mut enc).0, NVENCSTATUS::NV_ENC_SUCCESS);
                    }
                    cbr_row(
                        &format!("{content}: VBV {vbv_frames} frame(s), {name}"),
                        &mut enc,
                        Codec::H264,
                        seq,
                        w,
                        h,
                        fps,
                    );
                }
            }
        }
    }

    /// The reason HEVC pins High tier: with the level pinned rather than autoselected,
    /// NVENC validates the requested bitrate against that level's MaxBR, and Main tier's
    /// ceiling is low enough that an ordinary 4K desktop session runs into it.
    ///
    /// The test opens the same CBR HEVC session twice, once at each tier, at a bitrate
    /// above the Main-tier ceiling for the pinned level. Tier 1 must open. Tier 0 is
    /// reported rather than asserted: what the driver does above the ceiling is the thing
    /// being measured, and a future driver that stops rejecting it should not turn this
    /// into a red test, it should just make the printed line say so.
    #[test]
    #[ignore]
    fn gpu_hevc_high_tier_opens_above_the_main_tier_ceiling() {
        let mut s = settings(3840, 2160, 60.0);
        s.codec = Codec::H265;
        s.video_cbr_mode = true;
        // Above the Main-tier MaxBR of every 5.x level (40 Mbit/s at 5.1, 60 at 5.2).
        s.video_bitrate_kbps = 100_000;

        let high = host_session(&s);
        match &high {
            Ok(enc) => unsafe {
                let c = enc.encode_config.encodeCodecConfig.hevcConfig;
                assert_eq!(c.tier, 1);
                println!(
                    "HEVC High tier at {} kbps: opened, level {}",
                    s.video_bitrate_kbps, c.level,
                );
            },
            Err(e) if e.contains("engine") => {
                println!("H265: {e}");
                return;
            }
            Err(e) => panic!("High tier must open above the Main-tier ceiling: {e}"),
        }
        drop(high);

        // The same session at Main tier, through the tuned constructor so only the tier differs.
        let main = NvencEncoder::new_tuned(
            &s,
            ptr::null(),
            NvencTuning {
                hevc_high_tier: false,
                ..NvencTuning::default()
            },
        );
        match main {
            Ok(enc) => unsafe {
                println!(
                    "HEVC Main tier at {} kbps: opened on level {}",
                    s.video_bitrate_kbps, enc.encode_config.encodeCodecConfig.hevcConfig.level,
                )
            },
            Err(e) => println!(
                "HEVC Main tier at {} kbps: refused, the Main-tier ceiling: {e}",
                s.video_bitrate_kbps,
            ),
        }
    }

    /// On a real GPU of two or more engines under API 12.1 or later, an AV1 session splits its
    /// frames at 1080p, in three strips on three engines, and an HEVC one only while a resize
    /// holds it at 4K, and the split frames decode to the picture painted; on one engine
    /// nothing splits. Ignored by default.
    #[test]
    #[ignore]
    fn gpu_split_frame_follows_the_codec_and_picture() {
        use crate::webcam::decode::VideoDecoder;
        use NV_ENC_SPLIT_ENCODE_MODE as Split;
        for codec in [Codec::H265, Codec::Av1] {
            let mut s = settings(1920, 1080, 60.0);
            s.codec = codec;
            let mut enc = match host_session(&s) {
                Ok(enc) => enc,
                Err(e) if e.contains("engine") => {
                    println!("{codec:?}: {e}");
                    continue;
                }
                Err(e) => panic!("{codec:?}: {e}"),
            };
            if !enc.engines.is_some_and(|n| n > 1) || nvenc_cur_ver() < (12, 1) {
                println!(
                    "{codec:?}: {}, API {:?}",
                    enc.split_summary(),
                    nvenc_cur_ver()
                );
                assert_eq!(
                    enc.init_params.splitEncodeMode(),
                    Split::NV_ENC_SPLIT_AUTO_MODE as u32
                );
                continue;
            }
            for (n, (w, h)) in [(1920usize, 1080usize), (3840, 2160), (1920, 1080)]
                .into_iter()
                .enumerate()
            {
                if n > 0 {
                    s.width = w as i32;
                    s.height = h as i32;
                    assert!(enc.reconfigure_resolution(&s).expect("resize"));
                }
                let want = match codec {
                    Codec::Av1 if enc.engines == Some(3) => Split::NV_ENC_SPLIT_THREE_FORCED_MODE,
                    Codec::Av1 => Split::NV_ENC_SPLIT_AUTO_FORCED_MODE,
                    _ if w * h >= 3840 * 2160 => Split::NV_ENC_SPLIT_AUTO_FORCED_MODE,
                    _ => Split::NV_ENC_SPLIT_AUTO_MODE,
                };
                assert_eq!(
                    enc.init_params.splitEncodeMode(),
                    want as u32,
                    "{codec:?} at {w}x{h}: {}",
                    enc.split_summary()
                );
                let mut dec = VideoDecoder::new(codec).expect("decoder");
                for i in 0..6usize {
                    let f = moving_frame(w, h, i);
                    let pkt = enc
                        .encode_cpu_packed(&f, w * 4, false, (n * 10 + i) as u64, 25, i == 0)
                        .expect("encode");
                    let psnr = luma_psnr(&mut dec, &pkt, &f, w, h);
                    assert!(psnr > 30.0, "{codec:?} {w}x{h} frame {i}: {psnr:.1} dB");
                }
                println!("{codec:?} {w}x{h}: {}", enc.split_summary());
            }
        }
    }

    /// On a real GPU, a 1080p60 CBR session whose target lies past the ceiling of the level the
    /// picture alone would declare (H.264 4.2 at 62.5 Mbit/s, HEVC 4.1 High at 50, AV1's
    /// headroom 5.1 at the 26.7 `NVENC_AV1_RATE` holds it to where the driver does) opens on the
    /// level the rate raises it to, and a live raise past the ceiling, to 200 Mbit/s, which AV1
    /// holds at 106.7, is taken rather than refused. Ignored by default.
    #[test]
    #[ignore]
    fn gpu_cbr_targets_past_the_picture_level_open() {
        for (codec, kbps, level) in [
            (Codec::H264, 100_000, 50),
            (Codec::H265, 60_000, 150),
            (Codec::Av1, 30_000, 14),
        ] {
            let mut s = settings(1920, 1080, 60.0);
            s.codec = codec;
            s.video_cbr_mode = true;
            s.video_bitrate_kbps = kbps;
            let mut enc = match host_session(&s) {
                Ok(enc) => enc,
                Err(e) if e.contains("engine") => {
                    println!("{codec:?}: {e}");
                    continue;
                }
                Err(e) => panic!("{codec:?} at {kbps} kbps must open on level {level}: {e}"),
            };
            let level = if codec == Codec::Av1 && !NVENC_AV1_WEIGHTED.load(Ordering::Relaxed) {
                13
            } else {
                level
            };
            assert_eq!(
                enc.declared_level(),
                level,
                "{codec:?} level at {kbps} kbps"
            );
            s.video_bitrate_kbps = 200_000;
            assert!(
                enc.reconfigure_rate(&s),
                "{codec:?} live raise to 200 Mbit/s refused"
            );
            assert!(
                enc.declared_level() > level,
                "{codec:?} level did not rise with the target"
            );
            println!(
                "{codec:?}: {kbps} kbps opened on level {level}; 200 Mbit/s live took level {}",
                enc.declared_level()
            );
        }
    }

    /// On a real GPU with AV1, a 30 Mbit/s session declares 5.1, Annex A's level for the rate,
    /// where the driver admits it, and where the driver holds AV1 to `NVENC_AV1_RATE` it opens on
    /// 5.2 after the refusal, as does a live raise from 8 Mbit/s, at a key frame; either session
    /// then codes a frame. Ignored by default.
    #[test]
    #[ignore]
    fn gpu_av1_declares_the_lowest_level_the_driver_admits() {
        let f = frame(1920, 1080, 30);
        for raised in [false, true] {
            NVENC_AV1_WEIGHTED.store(false, Ordering::Relaxed);
            let mut s = settings(1920, 1080, 60.0);
            s.codec = Codec::Av1;
            s.video_cbr_mode = true;
            s.video_bitrate_kbps = if raised { 8_000 } else { 30_000 };
            let mut enc = match host_session(&s) {
                Ok(enc) => enc,
                Err(e) if e.contains("engine") => {
                    println!("Av1: {e}");
                    return;
                }
                Err(e) => panic!("Av1 at {} kbps: {e}", s.video_bitrate_kbps),
            };
            enc.encode_cpu_argb(&f, 1920 * 4, 0, 25, true)
                .expect("encode");
            if raised {
                assert_eq!(enc.declared_level(), 13);
                s.video_bitrate_kbps = 30_000;
                assert!(
                    enc.reconfigure_rate(&s),
                    "the raise to 30 Mbit/s was refused"
                );
            }
            let weighted = NVENC_AV1_WEIGHTED.load(Ordering::Relaxed);
            assert_eq!(enc.declared_level(), if weighted { 14 } else { 13 });
            let pkt = enc
                .encode_cpu_argb(&f, 1920 * 4, 1, 25, false)
                .expect("encode at the level");
            if raised && weighted {
                assert!(
                    crate::encoders::codec::av1_is_key(&pkt[VIDEO_HEADER_LEN..]),
                    "a raised level reaches the decoder at a key frame"
                );
            }
            println!(
                "Av1 30 Mbit/s {}: level {} ({})",
                if raised { "raised from 8" } else { "opened" },
                enc.declared_level(),
                if weighted {
                    "the driver holds two thirds of Annex A"
                } else {
                    "Annex A"
                }
            );
        }
    }

    /// On a real GPU, a live reconfigure costs a key frame only where one is asked for: a plain
    /// CBR rate change and a paint-over quantizer change go on predicting, and a target past the
    /// level's bitrate ceiling raises it at a key frame. A driver negotiated below 12.2 reads the
    /// flags elsewhere, so run it under `PIXELFLUX_NVENC_MAX_API` at each negotiable version too.
    /// Ignored by default.
    #[test]
    #[ignore]
    fn gpu_reconfigures_cost_a_key_frame_only_where_asked() {
        let f = frame(1920, 1080, 30);
        let kind = |enc: &mut NvencEncoder, n: u64, crf: u32| {
            enc.encode_cpu_argb(&f, 1920 * 4, n, crf, n == 0)
                .expect("encode")[1]
                & 0x0f
        };
        let mut s = settings(1920, 1080, 60.0);
        s.video_cbr_mode = true;
        s.video_bitrate_kbps = 8000;
        let mut enc = host_session(&s).expect("NVENC init");
        kind(&mut enc, 0, 25);
        for (n, kbps) in [(1, 6000), (2, 8000), (3, 6000)] {
            s.video_bitrate_kbps = kbps;
            assert!(enc.reconfigure_rate(&s));
            assert_eq!(
                kind(&mut enc, n, 25),
                FRAME_DELTA,
                "{:?}: after the change to {kbps} kbit/s",
                nvenc_cur_ver()
            );
        }
        s.video_bitrate_kbps = 200_000;
        assert!(enc.reconfigure_rate(&s));
        assert_eq!(
            kind(&mut enc, 4, 25),
            FRAME_KEY,
            "{:?}: the raised level reaches the stream at a key frame",
            nvenc_cur_ver()
        );
        let mut enc = host_session(&settings(1920, 1080, 60.0)).expect("NVENC init");
        for n in 0..6u64 {
            let want = if n == 0 { FRAME_KEY } else { FRAME_DELTA };
            assert_eq!(
                kind(&mut enc, n, if n % 2 == 0 { 25 } else { 18 }),
                want,
                "{:?}: paint-over frame {n}",
                nvenc_cur_ver()
            );
        }
    }

    /// On a real GPU, print the device-memory cost of one 1080p session (via `nvidia-smi`),
    /// for measuring the reconfigure-headroom overhead. Ignored by default.
    #[test]
    #[ignore]
    fn gpu_vram_probe() {
        // The measurement wants the driver's own view of the device, which only nvidia-smi
        // gives from outside the session's context; a host or container without it reports that
        // rather than failing a test that cannot measure anything. Summed over every GPU it lists,
        // so the delta is the session's on whichever device it opened.
        fn used_mb() -> Option<i64> {
            let out = std::process::Command::new("nvidia-smi")
                .args(["--query-gpu=memory.used", "--format=csv,noheader,nounits"])
                .output()
                .ok()?;
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(|l| l.trim().parse::<i64>().ok())
                .sum()
        }
        let s = settings(1920, 1080, 60.0);
        let Some(before) = used_mb() else {
            println!("VRAM probe skipped: nvidia-smi answered nothing");
            return;
        };
        let mut enc = host_session(&s).expect("init");
        let f = frame(1920, 1080, 5);
        for i in 0..3u64 {
            enc.encode_cpu_argb(&f, 1920 * 4, i, 25, i == 0)
                .expect("encode");
        }
        println!(
            "VRAM delta for one 1080p session: {} MiB",
            used_mb().unwrap_or(before) - before
        );
    }

    /// On a real GPU, a session that starts taller than the default 2304 headroom (portrait
    /// 4K: 2160×4096, within NVENC's 4096 H.264 cap) takes its own size as the `maxEncode` ceiling
    /// and encodes at that resolution. Ignored by default.
    /// The VUI has to describe what the session actually emits, on a real GPU and through the
    /// same caps negotiation a session takes: BT.709 at limited range, which is what the kernel
    /// writes for 4:2:0 and what NVENC's own conversion follows for 4:4:4. Primaries and
    /// transfer follow the source, which is sRGB desktop pixels and therefore BT.709 too. A
    /// client that inverts the wrong matrix, or expands a limited-range frame as full-range,
    /// shifts color visibly.
    #[test]
    #[ignore]
    fn gpu_vui_describes_the_conversion() {
        for fullcolor in [false, true] {
            let mut s = settings(1280, 720, 60.0);
            s.video_fullcolor = fullcolor;
            let Some(enc) = codec_session(&s) else {
                continue;
            };
            // encodeCodecConfig is a union; a successful init leaves the H.264 arm live.
            let h264 = unsafe { &enc.encode_config.encodeCodecConfig.h264Config };
            let vui = &h264.h264VUIParameters;
            assert_eq!(
                vui.colourMatrix as u32,
                NV_ENC_VUI_MATRIX_COEFFS::NV_ENC_VUI_MATRIX_COEFFS_BT709 as u32,
                "fullcolor={fullcolor}"
            );
            assert_eq!(
                vui.colourPrimaries as u32,
                NV_ENC_VUI_COLOR_PRIMARIES::NV_ENC_VUI_COLOR_PRIMARIES_BT709 as u32
            );
            assert_eq!(
                vui.transferCharacteristics as u32,
                NV_ENC_VUI_TRANSFER_CHARACTERISTIC::NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709 as u32
            );
            assert_eq!(vui.videoFullRangeFlag, 0, "fullcolor={fullcolor}");
            assert_eq!(h264.chromaFormatIDC, if enc.is_fullcolor() { 3 } else { 1 });
        }
    }

    /// On a real GPU with a render node: the chroma convert reaches the zero-copy path too. A
    /// dmabuf painted with alternating single-pixel columns whose pair averages to gray comes
    /// back with neutral chroma, whichever way the driver mapped the import — pitch-linear, read
    /// in place, or a CUDA array, read through a texture. Neither case copies the RGB.
    /// Ignored by default.
    #[test]
    #[ignore]
    fn gpu_dmabuf_chroma_is_sited_at_the_block_center() {
        use crate::webcam::decode::{Codec, Decoder as _, VideoDecoder};
        let (w, h) = (256u32, 256u32);
        let s = settings(w as i32, h as i32, 60.0);
        let Some((gbm, mut renderer)) = gpu_render() else {
            return;
        };
        let egl_display = renderer.egl_context().display().get_display_handle().handle;
        let (_bo, dmabuf) = column_dmabuf(&gbm, &mut renderer, w, h);
        let mut enc = NvencEncoder::new(&s, egl_display).expect("NVENC init");
        assert!(enc.csc.is_some(), "this GPU took no chroma convert");
        let pkt = enc.encode(&dmabuf, 0, 20, true).expect("dmabuf encode");
        let mut dec = VideoDecoder::new(Codec::H264).expect("H.264 decoder");
        assert!(
            dec.decode(&pkt[VIDEO_HEADER_LEN..]).expect("decode"),
            "no picture"
        );
        let v = dec.frame().expect("decoded frame");
        let (mut su, mut sv) = (0.0f64, 0.0f64);
        let n = (v.chroma_height() * v.chroma_width()) as f64;
        for r in 0..v.chroma_height() {
            for c in 0..v.chroma_width() {
                let i = r * v.uv_stride + c;
                su += f64::from(v.u[i]);
                sv += f64::from(v.v[i]);
            }
        }
        let (u, cr) = (su / n, sv / n);
        println!(
            "[chroma-siting] dmabuf mapped as {}: ({u:.1}, {cr:.1})",
            mapped_kind(&enc)
        );
        let off = (u - 128.0).hypot(cr - 128.0);
        assert!(
            off <= 2.0,
            "the zero-copy path leaves chroma {off:.1} off neutral"
        );
    }

    /// A dmabuf painted with alternating single-pixel columns of blue and yellow, whose pair
    /// averages to gray.
    fn column_dmabuf(
        gbm: &gbm::Device<std::fs::File>,
        renderer: &mut smithay::backend::renderer::gles::GlesRenderer,
        w: u32,
        h: u32,
    ) -> (gbm::BufferObject<()>, Dmabuf) {
        use gbm::{BufferObjectFlags, Format as GbmFormat};
        use smithay::backend::renderer::{Bind, Color32F, Frame, Renderer};
        use smithay::utils::{Physical, Rectangle, Size, Transform};
        let bo = gbm
            .create_buffer_object::<()>(w, h, GbmFormat::Argb8888, BufferObjectFlags::RENDERING)
            .expect("GBM buffer");
        let mut dmabuf = crate::create_dmabuf_from_bo(&bo);
        {
            let mut fb = renderer.bind(&mut dmabuf).expect("bind dmabuf");
            let size: Size<i32, Physical> = (w as i32, h as i32).into();
            let mut frame = renderer
                .render(&mut fb, size, Transform::Normal)
                .expect("render");
            let full: Rectangle<i32, Physical> = Rectangle::from_size(size);
            frame
                .clear(Color32F::new(0.0, 0.0, 1.0, 1.0), &[full])
                .expect("clear");
            for x in (1..w as i32).step_by(2) {
                let column: Rectangle<i32, Physical> =
                    Rectangle::new((x, 0).into(), (1, h as i32).into());
                frame
                    .draw_solid(
                        column,
                        &[Rectangle::from_size(column.size)],
                        Color32F::new(1.0, 1.0, 0.0, 1.0),
                    )
                    .expect("draw column");
            }
            let sync = frame.finish().expect("finish");
            let _ = sync.wait();
        }
        (bo, dmabuf)
    }

    /// On a real GPU: a 4:2:0 session sites chroma at the center of the block on both axes.
    /// Rows of a color pair that averages to gray, and columns of the same pair, both come back
    /// neutral. NVENC's own conversion averages the rows but weights the columns 3:1, which is
    /// what `ChromaConvert` replaces; with the convert disabled the column case lands three
    /// quarters of the way to the left column's chroma, which is the color subpixel-antialiased
    /// text would keep on its glyph edges. That conversion follows the BT.709 the session
    /// declares, so the mix is compared against BT.709 here and against BT.601 in
    /// `gpu_hardware_conversion_matches_the_declared_matrix`, which declares that instead. A
    /// 4:4:4 session subsamples nothing, so it keeps the hardware conversion. Ignored by
    /// default.
    #[test]
    #[ignore]
    fn gpu_chroma_is_sited_at_the_block_center() {
        use crate::encoders::chroma_siting::{BT709, chroma};
        let (w, h) = (256usize, 256usize);
        let (blue, yellow) = ([0.0, 0.0, 255.0], [255.0, 255.0, 0.0]);
        let st = settings(w as i32, h as i32, 60.0);
        let mut enc = host_session(&st).expect("NVENC init");
        assert!(enc.csc.is_some(), "this GPU took no chroma convert");
        let rows = encode_and_measure(&mut enc, &color_pair(w, h, blue, yellow, false));
        let cols = encode_and_measure(&mut enc, &color_pair(w, h, blue, yellow, true));
        println!("[chroma-siting] convert: rows {rows:?} columns {cols:?}");
        for (label, (u, v)) in [("rows", rows), ("columns", cols)] {
            let off = (u - 128.0).hypot(v - 128.0);
            assert!(off <= 2.0, "{label} come back {off:.1} off neutral chroma");
        }
        unsafe {
            let cu = enc.cuda.clone();
            (cu.cuCtxPushCurrent_v2)(enc.cuda_context);
            if let Some(csc) = enc.csc.take() {
                csc.release(&cu, &enc.nvenc_funcs, enc.encoder_session);
            }
            (cu.cuCtxPopCurrent_v2)(ptr::null_mut());
        }
        let hardware = encode_and_measure(&mut enc, &color_pair(w, h, blue, yellow, true));
        let weighted = chroma([0, 1, 2].map(|i| 0.75 * blue[i] + 0.25 * yellow[i]), BT709);
        println!("[chroma-siting] hardware: columns {hardware:?} against 3:1 {weighted:?}");
        assert!(
            (hardware.0 - weighted.0).hypot(hardware.1 - weighted.1) <= 2.0,
            "NVENC's own conversion weights the columns 3:1: {hardware:?} against {weighted:?}"
        );

        let full = CaptureSettings {
            video_fullcolor: true,
            ..st
        };
        if let Some(enc444) = codec_session(&full)
            && enc444.is_fullcolor()
        {
            assert!(
                enc444.csc.is_none(),
                "a 4:4:4 session has no chroma to site and takes no convert"
            );
        }
    }

    /// On a real GPU, in both chroma formats: the color chart decodes back to the painted
    /// color when the matrix the VUI declares is inverted, which is what a client does with
    /// every frame. 4:2:0 comes through the kernel and 4:4:4 through NVENC's own conversion, so
    /// this is what holds both to the declared matrix — the siting check cannot see a wrong one,
    /// its tile being neutral whichever matrix converts it. OpenH264, the H.264 decoder the tests
    /// carry, refuses a 4:4:4 stream's SPS, so that stream is held to what its SPS declares and
    /// the 4:4:4 picture is read back from the HEVC session, which converts the same way. Ignored
    /// by default.
    #[test]
    #[ignore]
    fn gpu_chart_decodes_to_the_color_that_was_painted() {
        use crate::encoders::chroma_siting::{BT709, chart_bgra, chart_error};
        use crate::encoders::sps::{h264_chroma_format_idc, read_color};
        use crate::webcam::decode::{Decoder as _, VideoDecoder};
        let (w, h) = (256usize, 128usize);
        for (codec, fullcolor) in [
            (Codec::H264, false),
            (Codec::H264, true),
            (Codec::H265, true),
        ] {
            let st = CaptureSettings {
                codec,
                video_fullcolor: fullcolor,
                ..settings(w as i32, h as i32, 60.0)
            };
            let Some(mut enc) = codec_session(&st) else {
                continue;
            };
            if fullcolor && !enc.is_fullcolor() {
                println!("[chart] this GPU carries no 4:4:4 {codec:?}");
                continue;
            }
            assert_eq!(
                enc.csc.is_some(),
                !fullcolor,
                "the convert follows the chroma format"
            );
            let check = |enc: &mut NvencEncoder, bgra: &[u8], w: usize, frame: u64, what: &str| {
                let pkt = enc
                    .encode_cpu_packed(bgra, w * 4, false, frame, 20, true)
                    .expect("encode");
                let stream = &pkt[VIDEO_HEADER_LEN..];
                if codec == Codec::H264 && fullcolor {
                    assert_eq!(
                        h264_chroma_format_idc(stream),
                        Some(3),
                        "the SPS declares 4:4:4"
                    );
                    let sps = crate::encoders::codec::annexb_nals(stream)
                        .find(|n| n[0] & 0x1f == 7)
                        .expect("an SPS");
                    assert_eq!(
                        read_color(sps).map(|s| (s.matrix, s.full_range)),
                        Some((1, false)),
                        "the SPS declares BT.709 limited"
                    );
                    println!("[chart] NVENC H264 4:4:4{what}: the SPS declares 4:4:4 BT.709");
                    return;
                }
                let mut dec = VideoDecoder::new(codec).expect("decoder");
                assert!(dec.decode(stream).expect("decode"), "no picture");
                let worst = chart_error(&dec.frame().expect("decoded frame"), BT709);
                println!(
                    "[chart] NVENC {codec:?} 4:4:4 {fullcolor}{what}: worst |dRGB| {worst:.1}"
                );
                assert!(
                    worst <= 8.0,
                    "the session paints {worst:.1} off the chart{what}"
                );
            };
            check(&mut enc, &chart_bgra(w, h), w, 0, "");

            // A resize rebuilds the convert's surface around the new geometry, so the chart is
            // read back again at a size the session was not opened for.
            let (w2, h2) = (w + 64, h + 32);
            let grown = CaptureSettings {
                width: w2 as i32,
                height: h2 as i32,
                ..st
            };
            assert!(
                enc.reconfigure_resolution(&grown).expect("resize"),
                "the resize was taken"
            );
            assert_eq!(
                enc.csc.is_some(),
                !fullcolor,
                "the convert followed the resize"
            );
            check(&mut enc, &chart_bgra(w2, h2), w2, 1, " after a resize");
        }
    }

    /// On a real GPU, the path a driver that refuses the PTX takes: a session the kernel is
    /// kept out of still encodes, and NVENC's own conversion follows the BT.709 its VUI
    /// declares — the same session with the matrix declared as SMPTE170M measures the BT.601
    /// mix instead, which is how that dependency was established and why the fallback can
    /// declare a matrix at all. Only the siting differs, the columns landing on the 3:1
    /// weighting. Ignored by default.
    #[test]
    #[ignore]
    fn gpu_hardware_conversion_matches_the_declared_matrix() {
        use crate::encoders::chroma_siting::{BT601, BT709, chart_bgra, chart_error, chroma};
        use crate::webcam::decode::{Decoder as _, VideoDecoder};
        let (w, h) = (256usize, 256usize);
        let (blue, yellow) = ([0.0, 0.0, 255.0], [255.0, 255.0, 0.0]);
        let st = settings(w as i32, h as i32, 60.0);
        let tuning = NvencTuning {
            hardware_csc: true,
            ..Default::default()
        };
        let mut enc = NvencEncoder::new_tuned(&st, ptr::null(), tuning).expect("NVENC init");
        assert!(
            enc.csc.is_none(),
            "the session was asked for NVENC's own conversion"
        );

        let pkt = enc
            .encode_cpu_packed(&chart_bgra(w, h), w * 4, false, 0, 20, true)
            .expect("encode");
        let mut dec = VideoDecoder::new(Codec::H264).expect("H.264 decoder");
        assert!(
            dec.decode(&pkt[VIDEO_HEADER_LEN..]).expect("decode"),
            "no picture"
        );
        let worst = chart_error(&dec.frame().expect("decoded frame"), BT709);
        println!("[csc] NVENC's own conversion: chart worst |dRGB| {worst:.1}");
        assert!(
            worst <= 8.0,
            "the hardware conversion paints {worst:.1} off the declared matrix"
        );

        let hardware = encode_and_measure(&mut enc, &color_pair(w, h, blue, yellow, true));
        let mix = [0, 1, 2].map(|i| 0.75 * blue[i] + 0.25 * yellow[i]);
        let (weighted_601, weighted_709) = (chroma(mix, BT601), chroma(mix, BT709));
        let off = |c: (f64, f64)| (hardware.0 - c.0).hypot(hardware.1 - c.1);
        println!(
            "[csc] NVENC's own conversion: columns {hardware:?} against the 3:1 mix under \
             BT.709 {weighted_709:?} and BT.601 {weighted_601:?}"
        );
        assert!(
            off(weighted_709) <= 2.0 && off(weighted_709) < off(weighted_601),
            "the hardware conversion did not follow the declared matrix: {hardware:?}"
        );
    }

    /// On a real GPU: the external-pointer path encodes the caller's buffer where it lies. The
    /// session's own staging surface is painted a color the frame does not contain first, so a
    /// picture that had been copied through it would decode to that color instead — this is the
    /// X11 NvFBC hand-over, without needing NvFBC to reach it. A pitch that cannot cover the
    /// session's rows is refused rather than read past. Ignored by default.
    #[test]
    #[ignore]
    fn gpu_external_pointer_is_encoded_where_it_lies() {
        use crate::encoders::chroma_siting::{BT709, ycbcr};
        use crate::webcam::decode::VideoDecoder;
        let (w, h) = (256usize, 128usize);
        let st = settings(w as i32, h as i32, 60.0);
        let mut enc = host_session(&st).expect("NVENC init");
        let paint = [32u8, 192, 64];
        let poison = [240u8, 16, 200];
        let (external, external_pitch) = unsafe {
            let cu = enc.cuda.clone();
            (cu.cuCtxPushCurrent_v2)(enc.cuda_context);
            let (mut ptr_, mut pitch) = (0 as CUdeviceptr, 0usize);
            assert_eq!(
                (cu.cuMemAllocPitch_v2)(&mut ptr_, &mut pitch, w * 4, h, 16),
                CUresult::CUDA_SUCCESS
            );
            paint_surface(&cu, ptr_, pitch, w, h, paint);
            paint_surface(&cu, enc.input_device_ptr, enc.input_pitch, w, h, poison);
            (cu.cuCtxPopCurrent_v2)(ptr::null_mut());
            (ptr_, pitch)
        };
        let pkt = enc
            .encode_cuda_pitch(external, external_pitch, false, 0, 20, true)
            .expect("external encode");
        let mut dec = VideoDecoder::new(Codec::H264).expect("H.264 decoder");
        let (mean, _) = decoded_means(&mut dec, &pkt, (0, 0, w as i32, h as i32));
        let want = ycbcr(paint.map(f64::from), BT709);
        println!("[external] decoded {mean:?} against the painted {want:?}");
        for i in 0..3 {
            assert!(
                (mean[i] - want[i]).abs() <= 8.0,
                "plane {i} came back {:.1}, not the painted {:.1}: the frame was staged, not read in place",
                mean[i],
                want[i]
            );
        }
        assert!(
            enc.encode_cuda_pitch(external, w * 4 - 4, false, 1, 20, false)
                .is_err(),
            "a pitch too short for the session's rows must be refused"
        );
        enc.release_external_input();
        unsafe {
            let cu = enc.cuda.clone();
            (cu.cuCtxPushCurrent_v2)(enc.cuda_context);
            (cu.cuMemFree_v2)(external);
            (cu.cuCtxPopCurrent_v2)(ptr::null_mut());
        }
    }

    /// On a real GPU: a host frame asked to be hashed is hashed where it was uploaded, beside its
    /// encode, its band hashes taken with the frame: one a band, the same for the same picture,
    /// different only in the band a caret changed, and none for a frame that did not ask; and
    /// what that costs the encode. Ignored by default.
    #[test]
    #[ignore]
    fn gpu_an_uploaded_frame_is_hashed_beside_its_encode() {
        let (w, h, rows) = (1920usize, 1080usize, 32u32);
        let mut enc = host_session(&settings(w as i32, h as i32, 60.0)).expect("NVENC init");
        let mut picture = frame(w, h, 3);
        let encode = |enc: &mut NvencEncoder, picture: &[u8], n: u64, hash: bool| {
            if hash {
                assert!(enc.hash_next_upload(rows), "the driver takes the band hash");
            }
            let out = enc.encode_cpu_packed(picture, w * 4, false, n, 23, n == 0);
            assert!(out.is_ok_and(|o| !o.is_empty()), "frame {n} encodes");
            enc.take_upload_hashes()
        };
        let first = encode(&mut enc, &picture, 0, true).expect("hashes come with the frame");
        assert_eq!(first.len(), h.div_ceil(rows as usize));
        assert_eq!(
            encode(&mut enc, &picture, 1, true),
            Some(first.clone()),
            "one picture, one hash"
        );
        assert_eq!(encode(&mut enc, &picture, 2, false), None, "none unasked");
        for y in 200..220 {
            for x in 300..302 {
                picture[(y * w + x) * 4..][..3].copy_from_slice(&[0, 0, 0]);
            }
        }
        let caret = encode(&mut enc, &picture, 3, true).unwrap();
        let changed: Vec<usize> = (0..first.len()).filter(|&i| first[i] != caret[i]).collect();
        assert_eq!(changed, vec![200 / rows as usize], "a 2x20 caret");

        let mut timed = |hash: bool, from: u64| {
            let t = std::time::Instant::now();
            for n in from..from + 120 {
                picture[(n as usize % h) * w * 4] ^= 0xff;
                encode(&mut enc, &picture, n, hash);
            }
            t.elapsed().as_secs_f64() * 1e3 / 120.0
        };
        let (plain, hashed) = (timed(false, 4), timed(true, 124));
        let (plain2, hashed2) = (timed(false, 244), timed(true, 364));
        eprintln!(
            "1080p host encode: {:.3} / {:.3} ms plain, {:.3} / {:.3} ms with the device hash",
            plain, plain2, hashed, hashed2
        );
    }

    /// On a real GPU: the band hash reads a caret as the one band it is in and two pixels
    /// swapped as a change, but neither the alpha byte nor the pitch's padding as one; and what
    /// it costs a 1080p frame against the encode of it. Ignored by default.
    #[test]
    #[ignore]
    fn gpu_band_hash_reads_the_bands_that_changed() {
        let (w, h, rows) = (1920usize, 1080usize, 32usize);
        let pitch = w * 4 + 64;
        let mut enc = host_session(&settings(w as i32, h as i32, 60.0)).expect("NVENC init");
        let (cu, ctx) = (enc.cuda.clone(), enc.cuda_context);
        let picture = frame(w, h, 3);
        let mut host = vec![0u8; pitch * h];
        for y in 0..h {
            host[y * pitch..][..w * 4].copy_from_slice(&picture[y * w * 4..][..w * 4]);
        }
        let mut dev: CUdeviceptr = 0;
        unsafe {
            (cu.cuCtxPushCurrent_v2)(ctx);
            assert_eq!(
                (cu.cuMemAlloc_v2)(&mut dev, pitch * h),
                CUresult::CUDA_SUCCESS
            );
            (cu.cuCtxPopCurrent_v2)(ptr::null_mut());
        }
        let upload = |host: &[u8]| unsafe {
            (cu.cuCtxPushCurrent_v2)(ctx);
            assert_eq!(
                (cu.cuMemcpyHtoD_v2)(dev, host.as_ptr() as *const c_void, host.len()),
                CUresult::CUDA_SUCCESS
            );
            (cu.cuCtxPopCurrent_v2)(ptr::null_mut());
        };
        let changed = |a: &[u64], b: &[u64]| -> Vec<usize> {
            (0..a.len()).filter(|&i| a[i] != b[i]).collect()
        };
        upload(&host);
        let hash =
            |enc: &mut NvencEncoder| enc.band_hashes(dev, pitch, w as u32, h as u32, rows as u32);
        let jit = std::time::Instant::now();
        let first = hash(&mut enc).expect("the driver takes the band hash");
        let jit = jit.elapsed().as_secs_f64() * 1e3;
        assert_eq!(first.len(), h.div_ceil(rows));
        assert_eq!(hash(&mut enc).unwrap(), first, "one frame, one hash");

        for y in 200..220 {
            for x in 300..302 {
                host[y * pitch + x * 4..][..3].copy_from_slice(&[0, 0, 0]);
            }
        }
        upload(&host);
        let caret = hash(&mut enc).unwrap();
        assert_eq!(changed(&first, &caret), vec![200 / rows], "a 2x20 caret");

        for y in 0..h {
            for x in 0..w {
                host[y * pitch + x * 4 + 3] ^= 0x5a;
            }
            host[y * pitch + w * 4..][..64].fill(0xa5);
        }
        upload(&host);
        assert_eq!(
            hash(&mut enc).unwrap(),
            caret,
            "the alpha byte and the padding are not the picture"
        );

        let (a, b) = (650 * pitch + 100 * 4, 650 * pitch + 101 * 4);
        assert_ne!(host[a..a + 3], host[b..b + 3]);
        for i in 0..4 {
            host.swap(a + i, b + i);
        }
        upload(&host);
        assert_eq!(
            changed(&caret, &hash(&mut enc).unwrap()),
            vec![650 / rows],
            "two pixels swapped"
        );

        let mut time = |w: u32, h: u32| -> Vec<f64> {
            let mut ms: Vec<f64> = (0..300)
                .map(|_| {
                    let t = std::time::Instant::now();
                    enc.band_hashes(dev, pitch, w, h, rows as u32).unwrap();
                    t.elapsed().as_secs_f64() * 1e3
                })
                .collect();
            ms.sort_by(f64::total_cmp);
            ms
        };
        let (hashing, fixed) = (time(w as u32, h as u32), time(64, rows as u32));
        let mut encoding: Vec<f64> = (0..120u64)
            .map(|i| {
                let t = std::time::Instant::now();
                enc.encode_cuda_pitch(dev, pitch, false, i, 25, i == 0)
                    .expect("encode");
                t.elapsed().as_secs_f64() * 1e3
            })
            .collect();
        encoding.sort_by(f64::total_cmp);
        println!(
            "[band hash] 1080p: {:.3} ms median, {:.3} ms p99 a frame ({:.3} ms of it for a 64x{rows} \
             one), the first {jit:.1} ms with the module's load; the encode {:.3} ms median",
            hashing[150], hashing[297], fixed[150], encoding[60]
        );
        enc.release_external_input();
        unsafe {
            (cu.cuCtxPushCurrent_v2)(ctx);
            (cu.cuMemFree_v2)(dev);
            (cu.cuCtxPopCurrent_v2)(ptr::null_mut());
        }
    }

    /// Test helper: fill a pitched device surface with one BGRA color.
    unsafe fn paint_surface(
        cuda: &CudaFunctions,
        dst: CUdeviceptr,
        dst_pitch: usize,
        w: usize,
        h: usize,
        rgb: [u8; 3],
    ) {
        let mut host = vec![255u8; w * h * 4];
        for px in host.as_chunks_mut::<4>().0 {
            px[..3].copy_from_slice(&[rgb[2], rgb[1], rgb[0]]);
        }
        let copy = CUDA_MEMCPY2D {
            srcMemoryType: CUmemorytype::CU_MEMORYTYPE_HOST,
            srcHost: host.as_ptr() as *const c_void,
            srcPitch: w * 4,
            dstMemoryType: CUmemorytype::CU_MEMORYTYPE_DEVICE,
            dstDevice: dst,
            dstPitch: dst_pitch,
            WidthInBytes: w * 4,
            Height: h,
            ..Default::default()
        };
        assert_eq!(
            (cuda.cuMemcpy2D_v2)(&copy),
            CUresult::CUDA_SUCCESS,
            "paint the surface"
        );
    }

    /// A frame of two colors alternating by column or by row, so every 2x2 block averages to
    /// the gray between them.
    fn color_pair(w: usize, h: usize, a: [f64; 3], b: [f64; 3], by_column: bool) -> Vec<u8> {
        let mut f = vec![255u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let p = if (if by_column { x } else { y }) % 2 == 0 {
                    a
                } else {
                    b
                };
                f[(y * w + x) * 4..][..3].copy_from_slice(&[p[2] as u8, p[1] as u8, p[0] as u8]);
            }
        }
        f
    }

    /// Encode one key frame of `bgra` and return the mean chroma of the decoded picture.
    fn encode_and_measure(enc: &mut NvencEncoder, bgra: &[u8]) -> (f64, f64) {
        use crate::webcam::decode::{Decoder as _, VideoDecoder};
        let w = enc.width() as usize;
        let pkt = enc
            .encode_cpu_packed(bgra, w * 4, false, 0, 20, true)
            .expect("packed encode");
        let mut dec = VideoDecoder::new(Codec::H264).expect("H.264 decoder");
        assert!(
            dec.decode(&pkt[VIDEO_HEADER_LEN..]).expect("decode"),
            "no picture from this access unit"
        );
        let v = dec.frame().expect("decoded frame");
        let (mut su, mut sv) = (0.0f64, 0.0f64);
        let n = (v.chroma_height() * v.chroma_width()) as f64;
        for r in 0..v.chroma_height() {
            for c in 0..v.chroma_width() {
                let i = r * v.uv_stride + c;
                su += f64::from(v.u[i]);
                sv += f64::from(v.v[i]);
            }
        }
        (su / n, sv / n)
    }

    /// On a real GPU: what the chroma convert costs a frame, against NVENC converting the packed
    /// input itself, and how far its luma lands from the host convert's. Prints; ignored by default.
    #[test]
    #[ignore]
    fn gpu_bench_chroma_convert() {
        use crate::encoders::software::convert_to_yuv_mt;
        for (w, h) in [(1920usize, 1080usize), (3840, 2160)] {
            let st = settings(w as i32, h as i32, 60.0);
            let bgra = crate::encoders::chroma_siting::bgra(w, h);
            let mut kernel = host_session(&st).expect("NVENC init");
            assert!(kernel.csc.is_some());
            let with = bench_frames(&mut kernel, &bgra);
            let mut hardware = host_session(&st).expect("NVENC init");
            unsafe {
                let cu = hardware.cuda.clone();
                (cu.cuCtxPushCurrent_v2)(hardware.cuda_context);
                if let Some(csc) = hardware.csc.take() {
                    csc.release(&cu, &hardware.nvenc_funcs, hardware.encoder_session);
                }
                (cu.cuCtxPopCurrent_v2)(ptr::null_mut());
            }
            let without = bench_frames(&mut hardware, &bgra);
            let (cw, ch) = (w / 2, h / 2);
            let (mut y, mut u, mut v) = (vec![0u8; w * h], vec![0u8; cw * ch], vec![0u8; cw * ch]);
            let mut host = f64::MAX;
            for _ in 0..20 {
                let t = std::time::Instant::now();
                convert_to_yuv_mt(
                    &bgra,
                    (w * 4) as u32,
                    w,
                    h,
                    false,
                    false,
                    false,
                    false,
                    &mut y,
                    &mut u,
                    &mut v,
                    (w, cw),
                    8,
                )
                .expect("host convert");
                host = host.min(t.elapsed().as_secs_f64() * 1000.0);
            }
            println!(
                "[bench] {w}x{h}: encode {with:.2} ms with the convert, {without:.2} ms on NVENC's own; the host convert costs {host:.2} ms on 8 threads"
            );
        }
    }

    /// Mean per-frame `encode_cpu_packed` time over 60 frames, after a warm-up.
    fn bench_frames(enc: &mut NvencEncoder, bgra: &[u8]) -> f64 {
        let w = enc.width() as usize;
        for i in 0..10 {
            enc.encode_cpu_packed(bgra, w * 4, false, i, 25, i == 0)
                .expect("warm-up");
        }
        let t = std::time::Instant::now();
        for i in 0..60u64 {
            enc.encode_cpu_packed(bgra, w * 4, false, 10 + i, 25, false)
                .expect("encode");
        }
        t.elapsed().as_secs_f64() * 1000.0 / 60.0
    }

    #[test]
    #[ignore]
    fn gpu_init_above_default_headroom() {
        let s = settings(2160, 4096, 30.0);
        let mut enc = host_session(&s).expect("NVENC init portrait 4K");
        assert_eq!(enc.init_params.maxEncodeWidth, 4096);
        assert_eq!(enc.init_params.maxEncodeHeight, 4096);
        let f = frame(2160, 4096, 20);
        let pkt = enc
            .encode_cpu_argb(&f, 2160 * 4, 0, 25, true)
            .expect("encode portrait 4K");
        assert_eq!(wire_dims(&pkt), (2160, 4096));
    }

    /// Thread CPU time, for the per-frame CPU cost of an encode path independent of how long
    /// the thread waited on the GPU.
    fn thread_cpu() -> std::time::Duration {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
        std::time::Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
    }

    /// Test helper: run `f` `n` times and report wall and thread-CPU microseconds per call.
    fn per_frame(label: &str, n: usize, mut f: impl FnMut(usize)) -> (f64, f64) {
        let t0 = std::time::Instant::now();
        let c0 = thread_cpu();
        for i in 0..n {
            f(i);
        }
        let wall = t0.elapsed().as_secs_f64() * 1e6 / n as f64;
        let cpu = (thread_cpu() - c0).as_secs_f64() * 1e6 / n as f64;
        println!("{label}: {wall:.0} us wall/frame, {cpu:.0} us cpu/frame ({n} frames)");
        (wall, cpu)
    }

    /// Test helper: the raw GBM device and GLES renderer of the render node named by
    /// `PIXELFLUX_TEST_RENDER_NODE`, else the NVIDIA GPU's, brought up exactly as the compositor
    /// brings them up, or `None` where GBM allocates nothing there, as `probe_wayland_gpu`
    /// finds (NVIDIA's GBM backend arrived in driver 495), which the caller skips.
    pub(crate) fn gpu_render() -> Option<(
        gbm::Device<std::fs::File>,
        smithay::backend::renderer::gles::GlesRenderer,
    )> {
        let node = std::env::var("PIXELFLUX_TEST_RENDER_NODE")
            .ok()
            .or_else(|| crate::auto_select_render_node(Some("nvidia")))
            .expect("no NVIDIA render node");
        let (gbm, renderer) =
            crate::gpu_render_init(std::path::Path::new(&node)).expect("GPU render init");
        let bo = gbm.create_buffer_object::<()>(
            64,
            64,
            gbm::Format::Argb8888,
            gbm::BufferObjectFlags::RENDERING,
        );
        if let Err(e) = bo {
            println!("{node}: GBM allocates nothing ({e})");
            return None;
        }
        Some((gbm, renderer))
    }

    /// Background and block colors painted into test dmabufs, as `Color32F` components.
    pub(crate) const BG: [f32; 3] = [0.1, 0.2, 0.8];
    pub(crate) const FG: [f32; 3] = [0.9, 0.3, 0.1];

    /// Limited-range Y/Cb/Cr of a painted color, whose components are 0..1.
    pub(crate) fn painted_ycbcr(rgb: [f32; 3]) -> [f64; 3] {
        use crate::encoders::chroma_siting::{BT709, ycbcr};
        ycbcr(rgb.map(|c| f64::from(c) * 255.0), BT709)
    }

    /// Where the foreground block of a `seed`-painted `w×h` frame sits: a quarter-size block
    /// whose origin moves with the seed.
    pub(crate) fn block_rect(w: u32, h: u32, seed: u32) -> (i32, i32, i32, i32) {
        let x = ((seed * 37) % (w / 2)) as i32 & !1;
        let y = ((seed * 53) % (h / 2)) as i32 & !1;
        (x, y, (w / 4) as i32 & !1, (h / 4) as i32 & !1)
    }

    /// Test helper: allocate a `w×h` ARGB8888 render-target dmabuf on `gbm` and paint it with
    /// the GLES renderer — `BG` everywhere and an `FG` block at `block_rect(seed)` — waiting for
    /// the render to land before returning, as the compositor does before encoding.
    pub(crate) fn painted_dmabuf(
        gbm: &gbm::Device<std::fs::File>,
        renderer: &mut smithay::backend::renderer::gles::GlesRenderer,
        w: u32,
        h: u32,
        seed: u32,
    ) -> (gbm::BufferObject<()>, Dmabuf) {
        use gbm::{BufferObjectFlags, Format as GbmFormat};
        let bo = gbm
            .create_buffer_object::<()>(w, h, GbmFormat::Argb8888, BufferObjectFlags::RENDERING)
            .expect("GBM buffer");
        let mut dmabuf = crate::create_dmabuf_from_bo(&bo);
        paint_dmabuf(renderer, &mut dmabuf, w, h, seed);
        (bo, dmabuf)
    }

    /// Test helper: paint `dmabuf` (`w×h`) as `painted_dmabuf` does, over what it held.
    pub(crate) fn paint_dmabuf(
        renderer: &mut smithay::backend::renderer::gles::GlesRenderer,
        dmabuf: &mut Dmabuf,
        w: u32,
        h: u32,
        seed: u32,
    ) {
        use smithay::backend::renderer::{Bind, Color32F, Frame, Renderer};
        use smithay::utils::{Physical, Rectangle, Size, Transform};
        {
            let mut fb = renderer.bind(dmabuf).expect("bind dmabuf");
            let size: Size<i32, Physical> = (w as i32, h as i32).into();
            let mut frame = renderer
                .render(&mut fb, size, Transform::Normal)
                .expect("render");
            let full: Rectangle<i32, Physical> = Rectangle::from_size(size);
            frame
                .clear(Color32F::new(BG[0], BG[1], BG[2], 1.0), &[full])
                .expect("clear");
            let (x, y, bw, bh) = block_rect(w, h, seed);
            let block: Rectangle<i32, Physical> = Rectangle::new((x, y).into(), (bw, bh).into());
            frame
                .draw_solid(
                    block,
                    &[Rectangle::from_size(block.size)],
                    Color32F::new(FG[0], FG[1], FG[2], 1.0),
                )
                .expect("draw block");
            let sync = frame.finish().expect("finish");
            let _ = sync.wait();
        }
    }

    /// Test helper: decode one H.264 access unit (the bytes behind the wire header)
    /// with the crate's H.264 decoder and return the mean Y/Cb/Cr inside `rect` and outside it.
    pub(crate) fn decoded_means(
        dec: &mut crate::webcam::decode::VideoDecoder,
        pkt: &[u8],
        rect: (i32, i32, i32, i32),
    ) -> ([f64; 3], [f64; 3]) {
        use crate::webcam::decode::Decoder;
        assert!(
            dec.decode(&pkt[VIDEO_HEADER_LEN..]).expect("decode"),
            "no picture from this access unit"
        );
        let v = dec.frame().expect("decoded frame");
        let (rx, ry, rw, rh) = rect;
        let inside = |x: usize, y: usize| {
            x as i32 >= rx && (x as i32) < rx + rw && y as i32 >= ry && (y as i32) < ry + rh
        };
        let mut acc = [[0f64; 3]; 2];
        let mut cnt = [0f64; 2];
        for y in 0..v.height {
            for x in 0..v.width {
                let k = if inside(x, y) { 0 } else { 1 };
                acc[k][0] += v.y[y * v.y_stride + x] as f64;
                acc[k][1] += v.u[(y / 2) * v.uv_stride + x / 2] as f64;
                acc[k][2] += v.v[(y / 2) * v.uv_stride + x / 2] as f64;
                cnt[k] += 1.0;
            }
        }
        let mean = |k: usize| [acc[k][0] / cnt[k], acc[k][1] / cnt[k], acc[k][2] / cnt[k]];
        (mean(0), mean(1))
    }

    /// Assert decoded region means sit within `tol` of the limited-range values of the painted
    /// colors — a wrong pitch, byte order, or stale buffer lands far outside this.
    pub(crate) fn assert_painted(label: &str, block: [f64; 3], bg: [f64; 3], tol: f64) {
        let (eb, eg) = (painted_ycbcr(FG), painted_ycbcr(BG));
        for i in 0..3 {
            assert!(
                (block[i] - eb[i]).abs() <= tol,
                "{label}: block plane {i} = {:.1}, expected {:.1}",
                block[i],
                eb[i]
            );
            assert!(
                (bg[i] - eg[i]).abs() <= tol,
                "{label}: background plane {i} = {:.1}, expected {:.1}",
                bg[i],
                eg[i]
            );
        }
    }

    /// Whether every cached dmabuf import of `enc` is registered with NVENC in place.
    fn all_direct(enc: &NvencEncoder) -> bool {
        !enc.dmabuf_cache.is_empty()
            && enc
                .dmabuf_cache
                .values()
                .all(|c| matches!(c.input, DmaBufInput::Direct { .. }))
    }

    /// How the driver mapped the cached dmabuf imports of `enc`, for the test output.
    fn mapped_kind(enc: &NvencEncoder) -> &'static str {
        match enc
            .dmabuf_cache
            .values()
            .next()
            .map(|c| c.egl_frame.frame_type)
        {
            Some(CU_EGL_FRAME_TYPE_PITCH) => "pitch-linear",
            Some(CU_EGL_FRAME_TYPE_ARRAY) => "a CUDA array",
            _ => "an unknown frame kind",
        }
    }

    /// On a real GPU with a render node: two GLES-painted dmabufs encode through the dmabuf path
    /// and decode to the painted colors at the painted positions, first with the direct
    /// registration enabled (in place when the driver maps the import pitch-linear, otherwise the
    /// copy arm) and then with it disabled — the two streams must agree, and the decoded content
    /// of both must match the paint. Prints which path the driver gave. Ignored by default; needs
    /// a render node of the NVIDIA GPU.
    #[test]
    #[ignore]
    fn gpu_dmabuf_direct_and_copy_paths_decode_to_the_paint() {
        use crate::webcam::decode::{Codec, VideoDecoder};
        let (w, h) = (1920u32, 1080u32);
        let s = settings(w as i32, h as i32, 60.0);
        let Some((gbm, mut renderer)) = gpu_render() else {
            return;
        };
        let egl_display = renderer.egl_context().display().get_display_handle().handle;
        let bufs: Vec<_> = (1..=2u32)
            .map(|seed| (seed, painted_dmabuf(&gbm, &mut renderer, w, h, seed)))
            .collect();
        let mut enc = NvencEncoder::new(&s, egl_display).expect("NVENC init");

        let run = |enc: &mut NvencEncoder, label: &str| -> Vec<Vec<u8>> {
            let mut dec = VideoDecoder::new(Codec::H264).expect("H.264 decoder");
            let mut out = Vec::new();
            for i in 0..6u64 {
                let (seed, (_, dmabuf)) = &bufs[(i % 2) as usize];
                let pkt = enc.encode(dmabuf, i, 25, i == 0).expect("dmabuf encode");
                assert_eq!(wire_dims(&pkt), (w as u16, h as u16));
                let (block, bg) = decoded_means(&mut dec, &pkt, block_rect(w, h, *seed));
                assert_painted(&format!("{label} frame {i}"), block, bg, 6.0);
                out.push(pkt[VIDEO_HEADER_LEN..].to_vec());
            }
            out
        };

        // Painting the session's own staging surface a color neither frame contains proves the
        // direct arm never passes through it: a copied frame would decode to this instead.
        if all_direct(&enc) {
            unsafe {
                let cu = enc.cuda.clone();
                (cu.cuCtxPushCurrent_v2)(enc.cuda_context);
                paint_surface(
                    &cu,
                    enc.input_device_ptr,
                    enc.input_pitch,
                    w as usize,
                    h as usize,
                    [240, 16, 200],
                );
                (cu.cuCtxPopCurrent_v2)(ptr::null_mut());
            }
        }
        let direct = run(&mut enc, "direct");
        println!(
            "driver mapped the dmabuf {}: {}",
            mapped_kind(&enc),
            if all_direct(&enc) {
                "registered in place"
            } else {
                "direct registration unavailable, copy arm used"
            }
        );

        // The copy arm is driven from a session that never registered an import in place: a
        // same-size reconfigure keeps the geometry, and so keeps the imports it cached, since
        // only a real resolution change drains them.
        let mut copy_only = NvencEncoder::new(&s, egl_display).expect("NVENC init");
        copy_only.direct_dmabuf = false;
        let copied = run(&mut copy_only, "copy");
        assert!(!all_direct(&copy_only));
        let identical = direct.iter().zip(&copied).all(|(a, b)| a == b);
        println!(
            "direct vs copy streams: {} ({} vs {} bytes)",
            if identical {
                "byte-identical"
            } else {
                "differ"
            },
            direct.iter().map(Vec::len).sum::<usize>(),
            copied.iter().map(Vec::len).sum::<usize>()
        );
        if let Ok(dir) = std::env::var("NVENC_TEST_DUMP_DIR") {
            std::fs::write(format!("{dir}/dmabuf-direct.h264"), direct.concat()).unwrap();
            std::fs::write(format!("{dir}/dmabuf-copy.h264"), copied.concat()).unwrap();
        }
    }

    /// On a real GPU with a render node: a 10-bit HEVC session takes dmabufs through the convert
    /// at either chroma, registered in place and through the copy arm, and the two arms code
    /// the same stream. Skips a device that lists no 10-bit HEVC. `NVENC_TEST_DUMP_DIR` keeps
    /// the streams for a decoder that takes them. Ignored by default.
    #[test]
    #[ignore]
    fn gpu_dmabuf_ten_bit_arms_agree() {
        let (w, h) = (1920u32, 1080u32);
        let Some((gbm, mut renderer)) = gpu_render() else {
            return;
        };
        let egl_display = renderer.egl_context().display().get_display_handle().handle;
        let bufs: Vec<_> = (1..=2u32)
            .map(|seed| painted_dmabuf(&gbm, &mut renderer, w, h, seed))
            .collect();
        for fullcolor in [false, true] {
            let mut s = settings(w as i32, h as i32, 60.0);
            s.codec = Codec::H265;
            s.video_fullcolor = fullcolor;
            s.video_bit_depth = 10;
            let mut streams = Vec::new();
            for direct in [true, false] {
                let mut enc = match NvencEncoder::new(&s, egl_display) {
                    Ok(enc) => enc,
                    Err(e) if e.contains("engine") => {
                        println!("{e}");
                        return;
                    }
                    Err(e) => panic!("NVENC init: {e}"),
                };
                if enc.bit_depth() != 10 {
                    println!("this device codes no 10-bit HEVC");
                    return;
                }
                enc.direct_dmabuf = direct;
                let stream: Vec<u8> = (0..6u64)
                    .flat_map(|i| {
                        let (_, dmabuf) = &bufs[(i % 2) as usize];
                        let pkt = enc.encode(dmabuf, i, 25, i == 0).expect("dmabuf encode");
                        pkt[VIDEO_HEADER_LEN..].to_vec()
                    })
                    .collect();
                println!(
                    "4:4:4 {fullcolor} direct {direct}: mapped {}, {} bytes",
                    mapped_kind(&enc),
                    stream.len()
                );
                if let Ok(dir) = std::env::var("NVENC_TEST_DUMP_DIR") {
                    std::fs::write(
                        format!("{dir}/dmabuf-10bit-{fullcolor}-{direct}.h265"),
                        &stream,
                    )
                    .unwrap();
                }
                streams.push(stream);
            }
            assert_eq!(streams[0], streams[1], "4:4:4 {fullcolor}: the arms differ");
        }
    }

    /// On a real GPU: a host frame handed over as BGRA (`rgba_input = false`) and the same image
    /// handed over as RGBA bytes (`rgba_input = true`) both decode to the painted colors — the
    /// input surface is re-registered in the other byte order in place — and the session keeps
    /// encoding across the switch. Ignored by default.
    #[test]
    #[ignore]
    fn gpu_packed_bgra_and_rgba_inputs_agree() {
        use crate::webcam::decode::{Codec, VideoDecoder};
        let (w, h) = (1280u32, 720u32);
        let s = settings(w as i32, h as i32, 60.0);
        let rect = block_rect(w, h, 3);
        let paint = |rgba: bool| -> Vec<u8> {
            let to_u8 = |c: f32| (c * 255.0).round() as u8;
            let mut f = vec![0u8; (w * h * 4) as usize];
            for y in 0..h as i32 {
                for x in 0..w as i32 {
                    let inside =
                        x >= rect.0 && x < rect.0 + rect.2 && y >= rect.1 && y < rect.1 + rect.3;
                    let c = if inside { FG } else { BG };
                    let px = &mut f[((y as u32 * w + x as u32) * 4) as usize..][..4];
                    let (r, g, b) = (to_u8(c[0]), to_u8(c[1]), to_u8(c[2]));
                    if rgba {
                        px.copy_from_slice(&[r, g, b, 255]);
                    } else {
                        px.copy_from_slice(&[b, g, r, 255]);
                    }
                }
            }
            f
        };
        let bgra = paint(false);
        let rgba = paint(true);
        let mut enc = host_session(&s).expect("NVENC init");
        let mut dec = VideoDecoder::new(Codec::H264).expect("H.264 decoder");
        let stride = (w * 4) as usize;
        // Byte order reaches the chroma convert as its own argument and the hardware conversion
        // as the packed surface's registered format, so both mechanisms are driven here: the
        // second pass is the path a GPU whose driver refuses the kernel takes.
        let pass = |enc: &mut NvencEncoder, dec: &mut VideoDecoder, tag: &str| {
            for (i, (buf, is_rgba)) in
                [(&bgra, false), (&rgba, true), (&bgra, false), (&rgba, true)]
                    .into_iter()
                    .enumerate()
            {
                let pkt = enc
                    .encode_cpu_packed(buf, stride, is_rgba, i as u64, 25, i == 0)
                    .expect("packed encode");
                if enc.csc.is_none() {
                    assert_eq!(
                        enc.input_format,
                        if is_rgba {
                            NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ABGR
                        } else {
                            NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB
                        }
                    );
                }
                let (block, bg) = decoded_means(dec, &pkt, rect);
                assert_painted(&format!("{tag} frame {i} rgba={is_rgba}"), block, bg, 6.0);
            }
        };
        pass(&mut enc, &mut dec, "convert");
        unsafe {
            let cu = enc.cuda.clone();
            (cu.cuCtxPushCurrent_v2)(enc.cuda_context);
            if let Some(csc) = enc.csc.take() {
                csc.release(&cu, &enc.nvenc_funcs, enc.encoder_session);
            }
            (cu.cuCtxPopCurrent_v2)(ptr::null_mut());
        }
        pass(&mut enc, &mut dec, "hardware");
    }

    /// On a real GPU: per-frame wall time of every NVENC preset, rate-control pass mode, and
    /// adaptive quantization, 1080p CBR on both codecs. Prints all; ignored by default.
    #[test]
    #[ignore]
    fn gpu_bench_tuning() {
        let (w, h) = (1920u32, 1080u32);
        let n: usize = std::env::var("NVENC_BENCH_FRAMES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(240);
        let mut s = settings(w as i32, h as i32, 60.0);
        s.video_cbr_mode = true;
        s.video_bitrate_kbps = 8000;
        let frames: Vec<Vec<u8>> = (0..8u8)
            .map(|k| frame(w as usize, h as usize, 10 + 30 * k))
            .collect();
        let stride = (w * 4) as usize;
        let presets = [
            ("P1", NV_ENC_PRESET_P1_GUID),
            ("P2", NV_ENC_PRESET_P2_GUID),
            ("P3", NV_ENC_PRESET_P3_GUID),
            ("P4", NV_ENC_PRESET_P4_GUID),
            ("P5", NV_ENC_PRESET_P5_GUID),
            ("P6", NV_ENC_PRESET_P6_GUID),
            ("P7", NV_ENC_PRESET_P7_GUID),
        ];
        let passes = [
            ("single pass", NV_ENC_MULTI_PASS::NV_ENC_MULTI_PASS_DISABLED),
            (
                "two-pass quarter",
                NV_ENC_MULTI_PASS::NV_ENC_TWO_PASS_QUARTER_RESOLUTION,
            ),
            (
                "two-pass full",
                NV_ENC_MULTI_PASS::NV_ENC_TWO_PASS_FULL_RESOLUTION,
            ),
        ];
        let mut cases: Vec<(String, NvencTuning)> = Vec::new();
        for (name, preset) in presets {
            cases.push((
                format!("{name} two-pass quarter"),
                NvencTuning {
                    preset,
                    ..NvencTuning::default()
                },
            ));
        }
        for (name, multipass) in passes {
            cases.push((
                format!("P4 {name}"),
                NvencTuning {
                    multipass,
                    ..NvencTuning::default()
                },
            ));
        }
        cases.push((
            "P4 two-pass quarter spatial AQ".into(),
            NvencTuning {
                spatial_aq: true,
                ..NvencTuning::default()
            },
        ));
        cases.push((
            "P4 two-pass quarter temporal AQ".into(),
            NvencTuning {
                temporal_aq: true,
                ..NvencTuning::default()
            },
        ));
        for codec in [Codec::H264, Codec::H265] {
            s.codec = codec;
            for (label, tuning) in &cases {
                let mut enc = match NvencEncoder::new_tuned(&s, ptr::null(), *tuning) {
                    Ok(enc) => enc,
                    Err(e) => {
                        println!("{codec:?} {label}: {e}");
                        continue;
                    }
                };
                enc.encode_cpu_packed(&frames[0], stride, false, 0, 25, true)
                    .expect("warm-up");
                let mut bytes = 0usize;
                per_frame(&format!("{codec:?} {label}"), n, |i| {
                    bytes += enc
                        .encode_cpu_packed(&frames[i % 8], stride, false, 1 + i as u64, 25, false)
                        .expect("encode")
                        .len();
                });
                println!("    {} kbps", bytes * 8 * 60 / n / 1000);
            }
        }
    }

    /// On a real GPU: the production tuning against the presets P1 and P2, one L0 reference, and
    /// each split-frame mode, on real content, per codec at 1080p, 1440p, and 2160p at CBR 8, 14,
    /// and 30 Mbit/s: a page of text scrolling four rows a frame and a texture panned six and
    /// three pixels. Per row the encode time at the median and the 99th percentile, the bitrate
    /// against the target, and the luma PSNR. `NVENC_BENCH_HEIGHTS` (1080,1440,2160) and
    /// `NVENC_BENCH_CODECS` (H264,H265,Av1) pick rows, `NVENC_BENCH_FRAMES` their length. Prints
    /// all; ignored by default.
    #[test]
    #[ignore]
    fn gpu_bench_tuning_on_content() {
        use crate::webcam::decode::VideoDecoder;
        let n: usize = std::env::var("NVENC_BENCH_FRAMES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(90);
        let glyph = |c: u32, gx: usize, gy: usize| {
            let s = c.wrapping_mul(2654435761) >> 7;
            (s & 1 == 1 && gx == 1)
                || (s & 2 == 2 && gx == 5)
                || (s & 4 == 4 && gy == 1)
                || (s & 8 == 8 && gy == 6)
                || (s & 16 == 16 && gy == 11)
                || (s & 32 == 32 && gx * 2 == gy)
        };
        let only = |var: &str, item: String| {
            std::env::var(var).map_or(true, |v| v.split(',').any(|x| x == item))
        };
        use NV_ENC_SPLIT_ENCODE_MODE as Split;
        for (w, h, kbps) in [
            (1920usize, 1080usize, 8000),
            (2560, 1440, 14000),
            (3840, 2160, 30000),
        ] {
            if !only("NVENC_BENCH_HEIGHTS", h.to_string()) {
                continue;
            }
            let mut page = vec![255u8; w * 2 * h * 4];
            for y in 0..2 * h {
                let (cy, gy) = (y / 18, y % 18);
                if cy % 9 == 8 || gy >= 13 {
                    continue;
                }
                for x in 0..w {
                    let (cx, gx) = (x / 10, x % 10);
                    let c = (cx as u32).wrapping_mul(31).wrapping_add(cy as u32 * 977);
                    if gx < 7 && (cx + cy * 3) % 13 != 0 && glyph(c, gx, gy) {
                        page[(y * w + x) * 4..(y * w + x) * 4 + 3].copy_from_slice(&[40, 30, 20]);
                    }
                }
            }
            let (tw, th) = (w * 2, h * 2);
            let mut tex = vec![0u8; tw * th * 4];
            for y in 0..th {
                for x in 0..tw {
                    let (fx, fy) = (x as f32, y as f32);
                    let v = 128.0
                        + 50.0 * (fx / 97.0).sin() * (fy / 61.0).cos()
                        + 30.0 * (fx / 13.0 + fy / 29.0).sin()
                        + ((x as u32).wrapping_mul(2654435761) ^ (y as u32).wrapping_mul(40503))
                            as f32
                            / u32::MAX as f32
                            * 24.0;
                    let i = (y * tw + x) * 4;
                    tex[i] = v as u8;
                    tex[i + 1] = (v * 0.8) as u8;
                    tex[i + 2] = (255.0 - v) as u8;
                }
            }
            let mut bufs = [vec![0u8; w * h * 4], vec![0u8; w * h * 4]];
            let draw = |scene: usize, t: usize, out: &mut Vec<u8>| {
                for y in 0..h {
                    let row = &mut out[y * w * 4..(y + 1) * w * 4];
                    if scene == 0 {
                        let src = (y + t * 4) % (2 * h);
                        row.copy_from_slice(&page[src * w * 4..(src + 1) * w * 4]);
                    } else {
                        let (ox, oy) = ((t * 6) % w, (y + t * 3) % h);
                        row.copy_from_slice(&tex[(oy * tw + ox) * 4..(oy * tw + ox + w) * 4]);
                    }
                }
            };
            for codec in [Codec::H264, Codec::H265, Codec::Av1] {
                if !only("NVENC_BENCH_CODECS", format!("{codec:?}")) {
                    continue;
                }
                let mut rows: Vec<(String, NvencTuning)> = vec![
                    ("production".into(), NvencTuning::default()),
                    (
                        "P1".into(),
                        NvencTuning {
                            preset: NV_ENC_PRESET_P1_GUID,
                            ..NvencTuning::default()
                        },
                    ),
                    (
                        "P2".into(),
                        NvencTuning {
                            preset: NV_ENC_PRESET_P2_GUID,
                            ..NvencTuning::default()
                        },
                    ),
                    (
                        "P3 L0=1".into(),
                        NvencTuning {
                            ref_l0: Some(NV_ENC_NUM_REF_FRAMES::NV_ENC_NUM_REF_FRAMES_1),
                            ..NvencTuning::default()
                        },
                    ),
                ];
                if codec != Codec::H264 {
                    for (name, split) in [
                        ("split off", Split::NV_ENC_SPLIT_DISABLE_MODE),
                        ("split driver's", Split::NV_ENC_SPLIT_AUTO_MODE),
                        ("split forced", Split::NV_ENC_SPLIT_AUTO_FORCED_MODE),
                        ("split two", Split::NV_ENC_SPLIT_TWO_FORCED_MODE),
                        ("split three", Split::NV_ENC_SPLIT_THREE_FORCED_MODE),
                    ] {
                        rows.push((
                            format!("P3 {name}"),
                            NvencTuning {
                                split: Some(split),
                                ..NvencTuning::default()
                            },
                        ));
                    }
                }
                for (scene, scene_name) in [(0usize, "text"), (1, "pan")] {
                    for (label, tuning) in &rows {
                        let mut s = settings(w as i32, h as i32, 60.0);
                        s.codec = codec;
                        s.video_cbr_mode = true;
                        s.video_bitrate_kbps = kbps;
                        let mut enc = match NvencEncoder::new_tuned(&s, ptr::null(), *tuning) {
                            Ok(enc) => enc,
                            Err(e) => {
                                println!("{codec:?} {w}x{h} {label}: {e}");
                                continue;
                            }
                        };
                        let engines = unsafe {
                            query_cap(
                                &enc.nvenc_funcs,
                                enc.encoder_session,
                                codec_guid(codec).unwrap(),
                                NV_ENC_CAPS::NV_ENC_CAPS_NUM_ENCODER_ENGINES,
                            )
                        };
                        let mut dec = VideoDecoder::new(codec).unwrap();
                        let (mut times, mut bytes, mut psnr) =
                            (Vec::with_capacity(n), 0usize, 0f64);
                        for t in 0..n + 5 {
                            let k = t % 2;
                            draw(scene, t, &mut bufs[k]);
                            let t0 = std::time::Instant::now();
                            let out = enc
                                .encode_cpu_packed(&bufs[k], w * 4, false, t as u64, 25, t == 0)
                                .expect("encode");
                            let ms = t0.elapsed().as_secs_f64() * 1e3;
                            let p = luma_psnr(&mut dec, &out, &bufs[k], w, h);
                            if t >= 5 {
                                times.push(ms);
                                bytes += out.len();
                                psnr += p;
                            }
                        }
                        times.sort_by(|a, b| a.total_cmp(b));
                        println!(
                            "TUNING {codec:?} {w}x{h} {scene_name} {label} (engines {engines:?}): encode {:.2} ms p50 {:.2} p99, {:.2} Mbit/s against {:.1}, PSNR {:.2} dB",
                            times[n / 2],
                            times[(n * 99) / 100],
                            bytes as f64 * 8.0 * 60.0 / n as f64 / 1e6,
                            kbps as f64 / 1000.0,
                            psnr / n as f64
                        );
                    }
                }
            }
        }
    }

    /// On a real GPU with a render node: per-frame wall and CPU cost of the dmabuf path with the
    /// in-place registration (when the driver maps the import pitch-linear) against the per-frame
    /// copy, 1080p, two painted buffers alternating. Prints both; ignored by default.
    #[test]
    #[ignore]
    fn gpu_bench_dmabuf_paths() {
        let (w, h) = (1920u32, 1080u32);
        let n: usize = std::env::var("NVENC_BENCH_FRAMES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(300);
        let s = settings(w as i32, h as i32, 60.0);
        let Some((gbm, mut renderer)) = gpu_render() else {
            return;
        };
        let egl_display = renderer.egl_context().display().get_display_handle().handle;
        let bufs: Vec<_> = (1..=2u32)
            .map(|seed| painted_dmabuf(&gbm, &mut renderer, w, h, seed).1)
            .collect();
        let mut enc = NvencEncoder::new(&s, egl_display).expect("NVENC init");
        for pass in 0..2 {
            enc.direct_dmabuf = pass == 0;
            enc.reconfigure_resolution(&s)
                .expect("reconfigure drains the import cache");
            enc.encode(&bufs[0], 0, 25, true).expect("warm-up");
            enc.encode(&bufs[1], 1, 25, false).expect("warm-up");
            let label = if all_direct(&enc) {
                format!("dmabuf registered in place ({})", mapped_kind(&enc))
            } else if pass == 0 {
                format!(
                    "dmabuf copy arm (direct registration unavailable, mapped {})",
                    mapped_kind(&enc)
                )
            } else {
                format!("dmabuf per-frame copy (mapped {})", mapped_kind(&enc))
            };
            per_frame(&label, n, |i| {
                enc.encode(&bufs[i % 2], 2 + i as u64, 25, false)
                    .expect("encode");
            });
        }
    }

    /// On a real GPU: per-frame wall and CPU cost of the readback→NVENC hand-over, 1080p host
    /// frames: the packed upload with the hardware CSC (BGRA and RGBA), and that same packed
    /// upload as a synchronous copy. Prints all; ignored by default.
    #[test]
    #[ignore]
    fn gpu_bench_readback_upload() {
        let (w, h) = (1920u32, 1080u32);
        let n: usize = std::env::var("NVENC_BENCH_FRAMES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(300);
        let s = settings(w as i32, h as i32, 60.0);
        let frames: Vec<Vec<u8>> = (0..4u8)
            .map(|k| frame(w as usize, h as usize, 10 + 40 * k))
            .collect();
        let stride = (w * 4) as usize;
        let mut enc = host_session(&s).expect("NVENC init");

        enc.reconfigure_resolution(&s).expect("reconfigure");
        enc.encode_cpu_packed(&frames[0], stride, false, 0, 25, true)
            .expect("warm-up");
        per_frame(
            "encode_cpu_packed BGRA (pinned, async upload, hardware CSC)",
            n,
            |i| {
                enc.encode_cpu_packed(&frames[i % 4], stride, false, 1 + i as u64, 25, false)
                    .expect("packed");
            },
        );

        enc.reconfigure_resolution(&s).expect("reconfigure");
        enc.encode_cpu_packed(&frames[0], stride, true, 0, 25, true)
            .expect("warm-up");
        per_frame(
            "encode_cpu_packed RGBA (pinned, async upload, hardware CSC)",
            n,
            |i| {
                enc.encode_cpu_packed(&frames[i % 4], stride, true, 1 + i as u64, 25, false)
                    .expect("packed");
            },
        );

        enc.reconfigure_resolution(&s).expect("reconfigure");
        enc.encode_cpu_packed(&frames[0], stride, false, 0, 25, true)
            .expect("warm-up");
        per_frame(
            "packed BGRA with a synchronous cuMemcpy2D upload",
            n,
            |i| unsafe {
                let _ = (enc.cuda.cuCtxPushCurrent_v2)(enc.cuda_context);
                let src = &frames[i % 4];
                enc.pin_host_source(src.as_ptr() as usize, src.len());
                let copy = CUDA_MEMCPY2D {
                    srcMemoryType: CUmemorytype::CU_MEMORYTYPE_HOST,
                    srcHost: src.as_ptr() as *const c_void,
                    srcPitch: stride,
                    dstMemoryType: CUmemorytype::CU_MEMORYTYPE_DEVICE,
                    dstDevice: enc.input_device_ptr,
                    dstPitch: enc.input_pitch,
                    WidthInBytes: stride,
                    Height: h as usize,
                    ..Default::default()
                };
                assert_eq!((enc.cuda.cuMemcpy2D_v2)(&copy), CUresult::CUDA_SUCCESS);
                enc.submit_frame(
                    enc.mapped_input_buffer,
                    enc.input_format,
                    1 + i as u64,
                    false,
                )
                .expect("submit");
                (enc.cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
            },
        );
    }

    /// How a bitstream lock waits for the encode: blocking in the driver, spinning on
    /// `doNotWait` locks, or sleeping between them.
    #[derive(Clone, Copy, Debug)]
    enum LockWait {
        Block,
        Spin,
        Sleep(std::time::Duration),
    }

    /// On a real GPU, the time from `nvEncEncodePicture` to a locked bitstream at 1080p under
    /// each lock strategy, with the CPU each spends and how many not-ready answers the driver
    /// gives: the spin marks when the encode really completes, the blocking lock shows what the
    /// driver's own wait adds, and each sleep interval what its quantization adds. Ignored by
    /// default.
    #[test]
    #[ignore]
    fn gpu_bench_lock_strategies() {
        let s = settings(1920, 1080, 60.0);
        let mut enc = host_session(&s).expect("NVENC init");
        let frames = [frame(1920, 1080, 10), frame(1920, 1080, 90)];
        let n = 240usize;
        let strategies = [
            LockWait::Spin,
            LockWait::Block,
            LockWait::Sleep(std::time::Duration::from_micros(50)),
            LockWait::Sleep(std::time::Duration::from_micros(200)),
            LockWait::Sleep(std::time::Duration::from_micros(1000)),
        ];
        for pass in 0..2 {
            for &strategy in &strategies {
                let mut waits: Vec<f64> = Vec::with_capacity(n);
                let mut busy_total = 0u64;
                let c0 = thread_cpu();
                for i in 0..n {
                    let (wait, busy) =
                        unsafe { lock_round_trip(&mut enc, &frames[i % 2], i as u64, strategy) };
                    waits.push(wait);
                    busy_total += busy;
                }
                let cpu_us = (thread_cpu() - c0).as_secs_f64() * 1e6 / n as f64;
                waits.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let mean = waits.iter().sum::<f64>() / n as f64;
                let pct = |q: f64| waits[((n as f64 - 1.0) * q) as usize];
                let empty = EMPTY_SUCCESSES.swap(0, std::sync::atomic::Ordering::Relaxed);
                if pass == 1 {
                    println!(
                        "{strategy:?}: submit->locked mean {mean:.0} us p50 {:.0} p95 {:.0} p99 {:.0} max {:.0}; {cpu_us:.0} us cpu/frame; {:.1} not-ready answers/frame; {:.2} empty successes/frame",
                        pct(0.5),
                        pct(0.95),
                        pct(0.99),
                        waits[n - 1],
                        busy_total as f64 / n as f64,
                        empty as f64 / n as f64
                    );
                }
            }
        }
    }

    /// Successful `doNotWait` locks that came back with an empty bitstream, across the bench.
    static EMPTY_SUCCESSES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// Test helper: upload one frame synchronously, submit it, wait for its bitstream with
    /// `strategy`, and unlock it; the wait in microseconds and the not-ready answers seen.
    unsafe fn lock_round_trip(
        enc: &mut NvencEncoder,
        pixels: &[u8],
        frame_number: u64,
        strategy: LockWait,
    ) -> (f64, u64) {
        let _ = (enc.cuda.cuCtxPushCurrent_v2)(enc.cuda_context);
        let copy = CUDA_MEMCPY2D {
            srcMemoryType: CUmemorytype::CU_MEMORYTYPE_HOST,
            srcHost: pixels.as_ptr() as *const c_void,
            srcPitch: (enc.width * 4) as usize,
            dstMemoryType: CUmemorytype::CU_MEMORYTYPE_DEVICE,
            dstDevice: enc.input_device_ptr,
            dstPitch: enc.input_pitch,
            WidthInBytes: (enc.width * 4) as usize,
            Height: enc.height as usize,
            ..Default::default()
        };
        assert_eq!((enc.cuda.cuMemcpy2D_v2)(&copy), CUresult::CUDA_SUCCESS);
        let output_bitstream = enc.bitstream_buffers[enc.current_buffer_idx];
        enc.current_buffer_idx = (enc.current_buffer_idx + 1) % enc.bitstream_buffers.len();
        let mut pic_params = NV_ENC_PIC_PARAMS {
            version: sv(NvStruct::PicParams),
            inputWidth: enc.width,
            inputHeight: enc.height,
            inputBuffer: enc.mapped_input_buffer,
            outputBitstream: output_bitstream,
            bufferFmt: enc.input_format,
            pictureStruct: NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME,
            encodePicFlags: if frame_number == 0 {
                NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_FORCEIDR as u32
            } else {
                0
            },
            ..Default::default()
        };
        let t0 = std::time::Instant::now();
        let st =
            (enc.nvenc_funcs.nvEncEncodePicture.unwrap())(enc.encoder_session, &mut pic_params);
        assert_eq!(st, NVENCSTATUS::NV_ENC_SUCCESS);
        let mut negotiated = Negotiated::new(NV_ENC_LOCK_BITSTREAM {
            version: sv(NvStruct::LockBitstream),
            outputBitstream: output_bitstream,
            ..Default::default()
        });
        let lock_params = &mut negotiated.value;
        lock_params.set_doNotWait(
            matches!(strategy, LockWait::Block)
                .then_some(0)
                .unwrap_or(1),
        );
        let lock_fn = enc.nvenc_funcs.nvEncLockBitstream.unwrap();
        let mut busy = 0u64;
        let mut empty_successes = 0u64;
        loop {
            match lock_fn(enc.encoder_session, lock_params) {
                NVENCSTATUS::NV_ENC_SUCCESS if lock_params.bitstreamSizeInBytes > 0 => break,
                NVENCSTATUS::NV_ENC_SUCCESS => {
                    empty_successes += 1;
                    (enc.nvenc_funcs.nvEncUnlockBitstream.unwrap())(
                        enc.encoder_session,
                        output_bitstream,
                    );
                    if let LockWait::Sleep(d) = strategy {
                        std::thread::sleep(d);
                    }
                    if matches!(strategy, LockWait::Block) {
                        panic!("blocking lock returned an empty bitstream");
                    }
                }
                NVENCSTATUS::NV_ENC_ERR_LOCK_BUSY => {
                    busy += 1;
                    if let LockWait::Sleep(d) = strategy {
                        std::thread::sleep(d);
                    }
                }
                other => panic!("lock failed: {other:?}"),
            }
        }
        let wait = t0.elapsed().as_secs_f64() * 1e6;
        if empty_successes > 0 {
            EMPTY_SUCCESSES.fetch_add(empty_successes, std::sync::atomic::Ordering::Relaxed);
        }
        (enc.nvenc_funcs.nvEncUnlockBitstream.unwrap())(enc.encoder_session, output_bitstream);
        (enc.cuda.cuCtxPopCurrent_v2)(ptr::null_mut());
        (wait, busy)
    }
}

#[cfg(test)]
mod version_tests {
    use super::*;

    /// Every version-tagged struct, in one fixed order, paired with its pinned compile-time
    /// `NV_ENC_*_VER` constant — the reference both version tests iterate.
    const ALL: [(NvStruct, u32); 13] = [
        (NvStruct::FunctionList, NV_ENCODE_API_FUNCTION_LIST_VER),
        (
            NvStruct::OpenSessionExParams,
            NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER,
        ),
        (NvStruct::Config, NV_ENC_CONFIG_VER),
        (NvStruct::RcParams, NV_ENC_RC_PARAMS_VER),
        (NvStruct::PresetConfig, NV_ENC_PRESET_CONFIG_VER),
        (NvStruct::InitializeParams, NV_ENC_INITIALIZE_PARAMS_VER),
        (NvStruct::ReconfigureParams, NV_ENC_RECONFIGURE_PARAMS_VER),
        (NvStruct::RegisterResource, NV_ENC_REGISTER_RESOURCE_VER),
        (NvStruct::MapInputResource, NV_ENC_MAP_INPUT_RESOURCE_VER),
        (
            NvStruct::CreateBitstreamBuffer,
            NV_ENC_CREATE_BITSTREAM_BUFFER_VER,
        ),
        (NvStruct::PicParams, NV_ENC_PIC_PARAMS_VER),
        (NvStruct::LockBitstream, NV_ENC_LOCK_BITSTREAM_VER),
        (NvStruct::CapsParam, NV_ENC_CAPS_PARAM_VER),
    ];

    /// For the pinned nvcodec-sys version, `nvenc_struct_ver` must reproduce every
    /// compile-time `NV_ENC_*_VER` constant exactly — guaranteeing a current driver is stamped
    /// byte-for-byte identically — and the packed major/minor must round-trip `NVENCAPI_VERSION`.
    /// Fails loudly if the bundled header is bumped without extending the revision table.
    #[test]
    fn table_is_identity_for_pinned_version() {
        let maj = NVENCAPI_VERSION & 0xFF;
        let min = (NVENCAPI_VERSION >> 24) & 0xFF;
        for (s, base) in ALL {
            assert_eq!(nvenc_struct_ver(s, maj, min), base, "{:?}", s);
        }
        assert_eq!(maj | (min << 24), NVENCAPI_VERSION);
    }

    /// A reconfigure's reset and IDR flags land where each version's own header puts them:
    /// behind initialize params of 1808 bytes below 12.2, and of 1800 from it.
    #[test]
    fn reconfigure_flags_sit_where_each_version_reads_them() {
        for (maj, min, at) in [
            (10, 0, 1816),
            (11, 0, 1816),
            (11, 1, 1816),
            (12, 0, 1816),
            (12, 1, 1816),
            (12, 2, 1808),
            (13, 0, 1808),
        ] {
            let params =
                reconfigure_params(NV_ENC_INITIALIZE_PARAMS::default(), (maj, min), true, true);
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    (&params as *const Negotiated<_>).cast::<u8>(),
                    std::mem::size_of_val(&params),
                )
            };
            let word = |at: usize| u32::from_ne_bytes(bytes[at..at + 4].try_into().unwrap());
            assert_eq!(
                (word(1808), word(1816)),
                if at == 1816 { (0, 3) } else { (3, 0) },
                "{maj}.{min}"
            );
        }
    }

    /// A QP delta map's pointer and size land where each version's own header puts them: at
    /// 1696 and 1704 below 12.0, and at 1712 and 1720 from it.
    #[test]
    fn qp_map_sits_where_each_version_reads_it() {
        let mut map = vec![0i8; 40];
        for (maj, min, at) in [
            (10, 0, 1696),
            (11, 0, 1696),
            (11, 1, 1696),
            (12, 0, 1712),
            (12, 1, 1712),
            (12, 2, 1712),
            (13, 0, 1712),
        ] {
            let mut pic = NV_ENC_PIC_PARAMS::default();
            point_at_qp_map(&mut pic, &mut map, (maj, min));
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    (&pic as *const NV_ENC_PIC_PARAMS).cast::<u8>(),
                    std::mem::size_of_val(&pic),
                )
            };
            let word = |at: usize| u64::from_ne_bytes(bytes[at..at + 8].try_into().unwrap());
            let other = if at == 1696 { 1712 } else { 1696 };
            assert_eq!(word(at), map.as_ptr() as u64, "{maj}.{min}");
            assert_eq!(word(at + 8) as u32, 40, "{maj}.{min}");
            assert_eq!((word(other), word(other + 8)), (0, 0), "{maj}.{min}");
        }
    }

    /// A 10-bit HEVC or AV1 config carries its depth where each version's own header puts it: at
    /// 12.2 and 13.0 as the input and output depth fields (10 at 200 and 204 for HEVC, 112 and
    /// 116 for AV1), before it as the depth minus eight in the bitfield at byte 17 (HEVC's
    /// `pixelBitDepthMinus8` at bits 3-5, AV1's input and output at 4-6 and 7-9), leaving the
    /// depth fields clear.
    #[test]
    fn bit_depth_sits_where_each_version_reads_it() {
        let word = |c: &NV_ENC_CONFIG, at: usize| {
            let bytes = unsafe {
                std::slice::from_raw_parts((c as *const NV_ENC_CONFIG).cast::<u8>(), 3584)
            };
            let at = 168 + at;
            u32::from_ne_bytes(bytes[at..at + 4].try_into().unwrap())
        };
        for (codec, depths, bits) in [
            (Codec::H265, (200, 204), 2 << 11),
            (Codec::Av1, (112, 116), 2 << 12 | 2 << 15),
        ] {
            for (maj, min) in [(10, 0), (11, 1), (12, 0), (12, 1), (12, 2), (13, 0)] {
                if codec == Codec::Av1 && maj < 12 {
                    continue;
                }
                let mut c = NV_ENC_CONFIG::default();
                set_bit_depth(&mut c, codec, 10, (maj, min));
                let named = (maj, min) >= (12, 2);
                assert_eq!(
                    (word(&c, depths.0), word(&c, depths.1)),
                    if named { (10, 10) } else { (0, 0) },
                    "{codec:?} {maj}.{min}"
                );
                assert_eq!(
                    word(&c, 16),
                    if named { 0 } else { bits },
                    "{codec:?} {maj}.{min}"
                );
            }
        }
    }

    /// The structs handed to the driver run as long as the longest layout a negotiable version
    /// gives them: the initialize params 1808 bytes and the reconfigure params around them 1824
    /// in the SDK 10.0 to 12.1 headers, and the lock params 1552 in the 12.1 one.
    #[test]
    fn negotiated_structs_hold_the_longest_layout() {
        assert_eq!(
            std::mem::size_of::<Negotiated<NV_ENC_INITIALIZE_PARAMS>>(),
            1808
        );
        assert_eq!(
            std::mem::size_of::<Negotiated<NV_ENC_RECONFIGURE_PARAMS>>(),
            1824
        );
        assert_eq!(
            std::mem::size_of::<Negotiated<NV_ENC_LOCK_BITSTREAM>>(),
            1552
        );
    }

    /// `nvenc_struct_ver` must reproduce the exact `NV_ENC_*_VER` words each SDK defined, for
    /// every negotiable version 10.0 through 13.0.
    ///
    /// The expected words are hardcoded from `nvEncodeAPI.h` at the FFmpeg nv-codec-headers tags
    /// listed in `NvStruct::rev`, one row per SDK version in `ALL` order. (The n10.0.26.2 header
    /// spells the flag `1<<31` rather than `1u<<31`; the bit is the same.) This is what lets the
    /// 13.0-layout structs be stamped with an older SDK's word when the session down-negotiates.
    #[test]
    fn table_matches_historical_headers() {
        #[rustfmt::skip]
        let expected: [(u32, u32, [u32; 12]); 7] = [
            (10, 0, [0x7002000A, 0x7001000A, 0xF007000A, 0x7001000A, 0xF004000A, 0xF005000A, 0xF001000A,
                     0x7003000A, 0x7004000A, 0x7001000A, 0xF004000A, 0x7001000A]),
            (11, 0, [0x7002000B, 0x7001000B, 0xF007000B, 0x7001000B, 0xF004000B, 0xF005000B, 0xF001000B,
                     0x7003000B, 0x7004000B, 0x7001000B, 0xF004000B, 0x7001000B]),
            (11, 1, [0x7102000B, 0x7101000B, 0xF107000B, 0x7101000B, 0xF104000B, 0xF105000B, 0xF101000B,
                     0x7103000B, 0x7104000B, 0x7101000B, 0xF104000B, 0x7101000B]),
            (12, 0, [0x7002000C, 0x7001000C, 0xF008000C, 0x7001000C, 0xF004000C, 0xF005000C, 0xF001000C,
                     0x7004000C, 0x7004000C, 0x7001000C, 0xF006000C, 0x7002000C]),
            (12, 1, [0x7102000C, 0x7101000C, 0xF108000C, 0x7101000C, 0xF104000C, 0xF106000C, 0xF101000C,
                     0x7104000C, 0x7104000C, 0x7101000C, 0xF106000C, 0xF101000C]),
            (12, 2, [0x7202000C, 0x7201000C, 0xF209000C, 0x7201000C, 0xF205000C, 0xF207000C, 0xF202000C,
                     0x7205000C, 0x7204000C, 0x7201000C, 0xF207000C, 0xF202000C]),
            (13, 0, [0x7002000D, 0x7001000D, 0xF009000D, 0x7001000D, 0xF005000D, 0xF007000D, 0xF002000D,
                     0x7005000D, 0x7004000D, 0x7001000D, 0xF007000D, 0xF002000D]),
        ];
        for (maj, min, words) in expected {
            for ((s, _), want) in ALL.iter().zip(words) {
                assert_eq!(
                    nvenc_struct_ver(*s, maj, min),
                    want,
                    "{:?} at {}.{}",
                    s,
                    maj,
                    min
                );
            }
        }
    }
}

#[cfg(test)]
mod decision_tests {
    use super::*;

    /// A caps query that returns `Some(0)` means the GPU lacks 4:4:4, so a 4:4:4 request is met
    /// with 4:2:0 and flagged as a downgrade; `Some(1)` keeps 4:4:4; an unqueryable cap (`None`)
    /// leaves the request untouched rather than downgrading on missing information.
    #[test]
    fn caps_chroma_downgrade() {
        let d = decide_caps(true, 1920, 1080, Some(0), None, None);
        assert!(!d.fullcolor && d.downgraded_color && d.too_large.is_none());

        let d = decide_caps(true, 1920, 1080, Some(1), None, None);
        assert!(d.fullcolor && !d.downgraded_color);

        let d = decide_caps(true, 1920, 1080, None, None, None);
        assert!(d.fullcolor && !d.downgraded_color);

        // A 4:2:0 request is never a downgrade whatever the cap says.
        let d = decide_caps(false, 1920, 1080, Some(0), None, None);
        assert!(!d.fullcolor && !d.downgraded_color);
    }

    /// A capture beyond the driver's reported maximum dimensions is flagged `too_large` (the
    /// caller then declines NVENC and uses software); a capture within them, or one whose caps are
    /// unknown, is not.
    #[test]
    fn caps_dimension_gate() {
        assert_eq!(
            decide_caps(false, 5120, 2160, None, Some(4096), Some(4096)).too_large,
            Some((4096, 4096))
        );
        assert_eq!(
            decide_caps(false, 3840, 4320, None, Some(4096), Some(4096)).too_large,
            Some((4096, 4096))
        );
        assert!(
            decide_caps(false, 3840, 2160, None, Some(4096), Some(4096))
                .too_large
                .is_none()
        );
        assert!(
            decide_caps(false, 7680, 4320, None, None, None)
                .too_large
                .is_none()
        );
        // A zero cap is treated as unknown, not as "everything is too large".
        assert!(
            decide_caps(false, 3840, 2160, None, Some(0), Some(0))
                .too_large
                .is_none()
        );
    }

    /// The resize headroom lifts the request to the 5.2-ceiling floor but never past the driver
    /// maximum, so initializing with headroom cannot itself exceed what the GPU supports.
    #[test]
    fn headroom_is_floored_and_capped() {
        assert_eq!(nvenc_headroom(1920, 4096, Some(8192)), 4096);
        assert_eq!(nvenc_headroom(3840, 4096, Some(4096)), 4096);
        assert_eq!(nvenc_headroom(6000, 4096, Some(8192)), 6000);
        assert_eq!(nvenc_headroom(1920, 4096, None), 4096);
        assert_eq!(nvenc_headroom(1920, 4096, Some(2048)), 2048);
    }

    /// H.264 and HEVC advertise the current geometry's level, so a decoder that gates on the
    /// level takes a 1080p stream a 4K-headroom session would once have marked 5.2; a resize
    /// re-declares it and a live rate change between the common rates does not. AV1 holds the
    /// headroom level, which NVENC requires it to cover at init and every AV1 decoder accepts.
    #[test]
    fn nvenc_level_follows_geometry_except_av1() {
        // The 1080p60 level, the one older Apple and Intel decoders gate on, is well below the
        // 4K-headroom level the session used to pin: H.264 4.2 and HEVC 4.1 rather than 5.2/5.1.
        assert_eq!(nvenc_level(Codec::H264, 1920, 1080, 60, 0, true), 42);
        assert_eq!(nvenc_level(Codec::H265, 1920, 1080, 60, 0, true), 123);
        // The current level rises with the picture, back to the headroom level at 4K.
        assert_eq!(nvenc_level(Codec::H264, 3840, 2160, 60, 0, true), 52);
        assert_eq!(nvenc_level(Codec::H265, 3840, 2160, 60, 0, true), 153);
        // A picture counts in whole coding tree blocks: 720p at 144 fps is 4.1 by its samples.
        assert_eq!(h265_level(1280, 720, 144, 0, true), 123);
        assert_eq!(nvenc_level(Codec::H265, 1280, 720, 144, 0, true), 150);
        // The current level admits the current picture on every codec.
        for (w, h) in [(1280u32, 720u32), (1920, 1080), (3840, 2160)] {
            let macroblocks = (w as u64 / 16) * (h as u64 / 16);
            assert!(
                h264_max_macroblocks(nvenc_level(Codec::H264, w, h, 60, 0, true)) >= macroblocks,
                "H.264 level at {w}x{h} cannot hold the picture"
            );
            assert!(
                h265_max_picture(nvenc_level(Codec::H265, w, h, 60, 0, true))
                    >= (w as u64) * (h as u64),
                "HEVC level at {w}x{h} cannot hold the picture"
            );
        }
        // AV1 holds the headroom level whatever the capture, since NVENC checks it against
        // maxEncodeWidth x maxEncodeHeight at init.
        let pixels = (HEADROOM_WIDTH * HEADROOM_HEIGHT) as u64;
        for (w, h) in [(1280, 720), (1920, 1080), (3840, 2160)] {
            assert_eq!(
                nvenc_level(Codec::Av1, w, h, 60, 0, true),
                nvenc_level(Codec::Av1, HEADROOM_WIDTH, HEADROOM_HEIGHT, 60, 0, true),
                "AV1 level moved with the capture at {w}x{h}"
            );
            assert!(
                av1_max_picture(nvenc_level(Codec::Av1, w, h, 60, 0, true)) >= pixels,
                "AV1 level at {w}x{h} cannot hold the headroom"
            );
        }
        // A live rate change between the common rates keeps the level, so it carries no IDR.
        for codec in [Codec::H264, Codec::H265, Codec::Av1] {
            assert_eq!(
                nvenc_level(codec, 1920, 1080, 30, 0, true),
                nvenc_level(codec, 1920, 1080, 60, 0, true),
                "{codec:?} level moved between 30 and 60 fps"
            );
        }
        // A CBR target past the level's ceiling takes the first level that admits it: the
        // driver refuses the session otherwise, as an invalid level.
        assert_eq!(
            nvenc_level(Codec::H264, 1920, 1080, 60, 100_000_000, true),
            50
        );
        assert_eq!(
            nvenc_level(Codec::H265, 1920, 1080, 60, 60_000_000, true),
            150
        );
        assert_eq!(
            nvenc_level(Codec::H265, 1920, 1080, 60, 60_000_000, false),
            156
        );
        // An AV1 level holds its Annex A MaxBitrate, 5.1 40 Mbit/s, 5.2 60, 6.1 100, 6.2 160;
        // weighed by `NVENC_AV1_RATE`, two thirds of it: 5.1 26.7, 5.2 40, 6.1 66.7, 6.2 106.7.
        for (bps, annex_a, weighted) in [
            (26_666_666, 13, 13),
            (26_666_667, 13, 14),
            (40_000_000, 13, 14),
            (40_000_001, 14, 17),
            (45_000_000, 14, 17),
            (60_000_001, 17, 17),
            (66_666_667, 17, 18),
            (100_000_001, 18, 18),
            (106_666_666, 18, 18),
        ] {
            for (weigh, level) in [(false, annex_a), (true, weighted)] {
                assert_eq!(
                    nvenc_level(
                        Codec::Av1,
                        1920,
                        1080,
                        60,
                        level_rate(Codec::Av1, bps, weigh),
                        true
                    ),
                    level,
                    "AV1 at {bps} bit/s, weighed {weigh}"
                );
            }
        }
        assert_eq!(level_rate(Codec::H265, 60_000_000, true), 60_000_000);
    }

    /// A frame splits across the engines for AV1 at any picture, in three strips on three, and
    /// for HEVC from 4K, never for H.264, on one engine, or before API 12.1, whose bits there are
    /// another flag's.
    #[test]
    fn split_mode_follows_the_codec_and_picture() {
        use NV_ENC_SPLIT_ENCODE_MODE::{
            NV_ENC_SPLIT_AUTO_FORCED_MODE as Forced, NV_ENC_SPLIT_AUTO_MODE as Driver,
            NV_ENC_SPLIT_THREE_FORCED_MODE as Three,
        };
        for (codec, w, h, engines, api, want) in [
            (Codec::Av1, 1280, 720, Some(2), (13, 0), Forced),
            (Codec::Av1, 3840, 2160, Some(3), (12, 1), Three),
            (Codec::Av1, 1920, 1080, Some(3), (13, 0), Three),
            (Codec::Av1, 1920, 1080, Some(4), (13, 0), Forced),
            (Codec::Av1, 1920, 1080, Some(3), (12, 0), Driver),
            (Codec::H265, 3840, 2160, Some(3), (13, 0), Forced),
            (Codec::H265, 1920, 1080, Some(3), (13, 0), Driver),
            (Codec::Av1, 1920, 1080, Some(1), (13, 0), Driver),
            (Codec::Av1, 1920, 1080, None, (13, 0), Driver),
            (Codec::H265, 2560, 1440, Some(2), (13, 0), Driver),
            (Codec::H265, 3840, 2160, Some(2), (13, 0), Forced),
            (Codec::H265, 2160, 3840, Some(2), (13, 0), Forced),
            (Codec::H265, 3840, 2160, Some(2), (12, 0), Driver),
            (Codec::H264, 3840, 2160, Some(2), (13, 0), Driver),
        ] {
            assert_eq!(
                split_mode(codec, w, h, engines, api),
                want,
                "{codec:?} {w}x{h} on {engines:?} engines, API {api:?}"
            );
        }
    }

    /// A CBR target past every level's ceiling is held to the top one, which the driver opens,
    /// and one inside it passes as asked.
    #[test]
    fn cbr_targets_hold_to_the_top_level() {
        for (codec, kbps, bps) in [
            (Codec::Av1, 8_000, 8_000_000),
            (Codec::Av1, 106_666, 106_666_000),
            (Codec::Av1, 200_000, 106_666_666),
            (Codec::H265, 800_000, 800_000_000),
            (Codec::H265, 1_000_000, 800_000_000),
            (Codec::H264, 1_000_000, 1_000_000_000),
            (Codec::H264, 5_000_000, 1_000_000_000),
        ] {
            let s = CaptureSettings {
                codec,
                video_bitrate_kbps: kbps,
                ..Default::default()
            };
            assert_eq!(cbr_bps(&s), bps, "{codec:?} at {kbps} kbps");
        }
        assert_eq!(
            nvenc_level(
                Codec::Av1,
                1920,
                1080,
                240,
                nvenc_rate_ceiling(Codec::Av1) as u64,
                true
            ),
            18
        );
    }

    /// AV1 Annex A MaxPicSize for a seq_level_idx the ladder can return.
    fn av1_max_picture(level: u32) -> u64 {
        match level {
            8 | 9 => 2_359_296,
            12..=15 => 8_912_896,
            _ => 35_651_584,
        }
    }

    /// HEVC Annex A MaxLumaPs for a general_level_idc the ladder can return.
    fn h265_max_picture(level: u32) -> u64 {
        match level {
            123 => 2_228_224,
            150 | 153 | 156 => 8_912_896,
            _ => 35_651_584,
        }
    }

    /// H.264 Annex A MaxFS for a level_idc the ladder can return.
    fn h264_max_macroblocks(level: u32) -> u64 {
        match level {
            41 => 8192,
            42 => 8704,
            50 => 22080,
            51 | 52 => 36864,
            _ => 139264,
        }
    }

    /// Test helper: a mapped `CUeglFrame` of one plane with the given kind, geometry, and pitch,
    /// its first plane at `plane` (a device pointer for the pitch kind, an array handle for the
    /// array kind) in four 8-bit channels.
    fn egl_frame(frame_type: u32, w: u32, h: u32, pitch: u32, plane: usize) -> CUeglFrame {
        let mut f: CUeglFrame = unsafe { std::mem::zeroed() };
        f.frame_type = frame_type;
        f.plane_count = 1;
        f.width = w;
        f.height = h;
        f.pitch = pitch;
        f.num_channels = 4;
        f.cu_format = CU_AD_FORMAT_U8;
        f.frame.p_pitch = [plane as *mut c_void, ptr::null_mut(), ptr::null_mut()];
        f
    }

    /// A pitch-linear mapping whose first plane covers the session geometry at a 4-byte-aligned
    /// pitch of at least `width * 4` is registered with NVENC in place as a device pointer at its
    /// own pitch; a null plane, a short or unaligned pitch, a frame smaller than the session, or a
    /// frame without planes each take the per-frame copy.
    #[test]
    fn pitch_linear_frame_direct_registration_rules() {
        let pitch_ok = egl_frame(CU_EGL_FRAME_TYPE_PITCH, 1920, 1080, 7680, 0x1000);
        assert_eq!(
            direct_plane(&pitch_ok, 1920, 1080),
            Some(DirectPlane::Pitch(7680))
        );
        let padded = egl_frame(CU_EGL_FRAME_TYPE_PITCH, 1920, 1080, 8192, 0x1000);
        assert_eq!(
            direct_plane(&padded, 1920, 1080),
            Some(DirectPlane::Pitch(8192))
        );
        let larger = egl_frame(CU_EGL_FRAME_TYPE_PITCH, 2048, 1200, 8192, 0x1000);
        assert_eq!(
            direct_plane(&larger, 1920, 1080),
            Some(DirectPlane::Pitch(8192))
        );

        let null_plane = egl_frame(CU_EGL_FRAME_TYPE_PITCH, 1920, 1080, 7680, 0);
        assert_eq!(direct_plane(&null_plane, 1920, 1080), None);
        let short_pitch = egl_frame(CU_EGL_FRAME_TYPE_PITCH, 1920, 1080, 7676, 0x1000);
        assert_eq!(direct_plane(&short_pitch, 1920, 1080), None);
        let unaligned = egl_frame(CU_EGL_FRAME_TYPE_PITCH, 1920, 1080, 7682, 0x1000);
        assert_eq!(direct_plane(&unaligned, 1920, 1080), None);
        let smaller = egl_frame(CU_EGL_FRAME_TYPE_PITCH, 1280, 720, 7680, 0x1000);
        assert_eq!(direct_plane(&smaller, 1920, 1080), None);
        let mut no_planes = pitch_ok;
        no_planes.plane_count = 0;
        assert_eq!(direct_plane(&no_planes, 1920, 1080), None);
        assert_eq!(direct_plane(&pitch_ok, 0, 1080), None);
        let mut unknown_kind = pitch_ok;
        unknown_kind.frame_type = 7;
        assert_eq!(direct_plane(&unknown_kind, 1920, 1080), None);
    }

    /// A CUDA-array mapping of four 8-bit channels covering the session geometry is registered in
    /// place as a CUDA array whose pitch word is the array's row width in bytes; an array of any
    /// other element layout, a null array, or one smaller than the session takes the copy.
    #[test]
    fn cuda_array_frame_direct_registration_rules() {
        let array = egl_frame(CU_EGL_FRAME_TYPE_ARRAY, 1920, 1080, 0, 0x2000);
        assert_eq!(
            direct_plane(&array, 1920, 1080),
            Some(DirectPlane::Array(7680))
        );
        let wider = egl_frame(CU_EGL_FRAME_TYPE_ARRAY, 2048, 1080, 0, 0x2000);
        assert_eq!(
            direct_plane(&wider, 1920, 1080),
            Some(DirectPlane::Array(8192))
        );

        let null_array = egl_frame(CU_EGL_FRAME_TYPE_ARRAY, 1920, 1080, 0, 0);
        assert_eq!(direct_plane(&null_array, 1920, 1080), None);
        let mut one_channel = array;
        one_channel.num_channels = 1;
        assert_eq!(direct_plane(&one_channel, 1920, 1080), None);
        let mut wide_elements = array;
        wide_elements.cu_format = 3;
        assert_eq!(direct_plane(&wide_elements, 1920, 1080), None);
        let smaller = egl_frame(CU_EGL_FRAME_TYPE_ARRAY, 1280, 720, 0, 0x2000);
        assert_eq!(direct_plane(&smaller, 1920, 1080), None);
    }

    /// The dmabuf fourcc picks the NVENC packed format with the same byte order: XR24 / AR24 are
    /// B,G,R,A in memory and so NVENC `ARGB`; XB24 / AB24 are R,G,B,A and so `ABGR`; anything else
    /// has no packed 8-bit NVENC equivalent.
    #[test]
    fn fourcc_selects_nvenc_byte_order() {
        for code in [Fourcc::Argb8888, Fourcc::Xrgb8888] {
            assert_eq!(
                fourcc_nvenc_format(code),
                Some(NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB)
            );
        }
        for code in [Fourcc::Abgr8888, Fourcc::Xbgr8888] {
            assert_eq!(
                fourcc_nvenc_format(code),
                Some(NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ABGR)
            );
        }
        for code in [
            Fourcc::Rgb565,
            Fourcc::Nv12,
            Fourcc::Argb2101010,
            Fourcc::Bgra8888,
            Fourcc::Rgba8888,
        ] {
            assert_eq!(fourcc_nvenc_format(code), None, "{code:?}");
        }
    }

    /// The EGL import names X-formats by their alpha twin and leaves everything else alone.
    #[test]
    fn egl_import_names_alpha_twins() {
        assert_eq!(egl_import_fourcc(Fourcc::Xrgb8888), Fourcc::Argb8888);
        assert_eq!(egl_import_fourcc(Fourcc::Xbgr8888), Fourcc::Abgr8888);
        for code in [Fourcc::Argb8888, Fourcc::Abgr8888, Fourcc::Nv12] {
            assert_eq!(egl_import_fourcc(code), code);
        }
    }
}
