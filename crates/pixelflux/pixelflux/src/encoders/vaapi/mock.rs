/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A libva stood in by Rust functions: a driver that offers what a test configures, records
//! every buffer a session renders, and hands back the packed headers as the coded picture, so
//! the driver-facing sequence of a session is checked where no VA-API device exists.

use std::cell::RefCell;
use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
use std::ptr;

use va_sys::*;

/// A rectangle as `(x, y, width, height)`.
pub(crate) type Rect = (i16, i16, u16, u16);

/// What the stood-in driver offers and what it saw.
#[derive(Default)]
pub(crate) struct Driver {
    pub profiles: Vec<VAProfile>,
    pub entrypoints: Vec<VAEntrypoint>,
    /// Configuration attributes by type, the same for every profile.
    pub attributes: Vec<(VAConfigAttribType, u32)>,
    /// Attributes one entry point reports in place of `attributes`.
    pub entrypoint_attributes: Vec<(VAEntrypoint, VAConfigAttribType, u32)>,
    /// The surface formats a configuration's surfaces may take.
    pub surface_fourccs: Vec<u32>,
    /// Whether `vaDeriveImage` answers.
    pub derive: bool,
    /// Whether `vaCreateImage` refuses.
    pub image_fails: bool,
    /// The pipeline color standards the video processor lists.
    pub color_standards: Vec<VAProcColorStandardType>,
    pub configs: Vec<(VAProfile, VAEntrypoint, Vec<VAConfigAttrib>)>,
    pub surfaces: Vec<(u32, u32, u32, u32)>,
    pub contexts: Vec<(VAConfigID, Vec<VASurfaceID>)>,
    /// Every buffer created: its context, type, and bytes.
    pub buffers: Vec<(VAContextID, VABufferType, Vec<u8>)>,
    /// Every picture rendered: its context, target surface, and the buffers rendered into
    /// it, by index into `buffers`.
    pub pictures: Vec<(VAContextID, VASurfaceID, Vec<usize>)>,
    /// The bytes handed back as the coded picture in place of what `coded_picture` builds.
    pub coded: Option<Vec<u8>>,
    /// The buffers of the images created or derived so far.
    pub images: Vec<VABufferID>,
    /// How many images were put onto surfaces.
    pub puts: u32,
    /// The source and output rectangles of every video-processing buffer, None for a null one.
    pub regions: Vec<(Option<Rect>, Option<Rect>)>,
    /// Every surface destroyed.
    pub destroyed: Vec<VASurfaceID>,
    /// The vendor string the driver answers, where not the stood-in one.
    pub vendor: Option<&'static CStr>,
    next_id: u32,
    in_picture: Option<(VAContextID, VASurfaceID, Vec<usize>)>,
    coded_out: Vec<u8>,
    segment: VACodedBufferSegment,
    terminated: u32,
}

thread_local! {
    static DRIVER: RefCell<Driver> = RefCell::new(Driver::default());
}

/// Run `f` on the stood-in driver of this thread.
pub(crate) fn with<R>(f: impl FnOnce(&mut Driver) -> R) -> R {
    DRIVER.with(|d| f(&mut d.borrow_mut()))
}

/// Start over with a driver offering `codecs` on every profile the session ladder names,
/// packed headers of every kind, both rate-control modes, and the usual attributes.
pub(crate) fn reset(offer: Driver) {
    DRIVER.with(|d| *d.borrow_mut() = offer);
}

impl Driver {
    /// A driver that serves every codec on the low-power entry point with every packed
    /// header, CBR and CQP, eight references, four slices of arbitrary rows, quality
    /// levels to seven, HEVC's default tools, AV1's usual attributes, and explicit color.
    pub(crate) fn generous() -> Self {
        let mut hevc_block = VAConfigAttribValEncHEVCBlockSizes { value: 0 };
        let mut hevc_features = VAConfigAttribValEncHEVCFeatures { value: 0 };
        let mut av1_ext2 = VAConfigAttribValEncAV1Ext2 { value: 0 };
        unsafe {
            hevc_block
                .bits
                .set_log2_max_coding_tree_block_size_minus3(2);
            hevc_block
                .bits
                .set_log2_min_luma_coding_block_size_minus3(0);
            hevc_block
                .bits
                .set_log2_max_luma_transform_block_size_minus2(3);
            hevc_block
                .bits
                .set_log2_min_luma_transform_block_size_minus2(0);
            hevc_block
                .bits
                .set_max_max_transform_hierarchy_depth_inter(2);
            hevc_block
                .bits
                .set_max_max_transform_hierarchy_depth_intra(2);
            hevc_features.bits.set_amp(1);
            hevc_features.bits.set_sao(1);
            hevc_features.bits.set_cu_qp_delta(1);
            av1_ext2.bits.set_tx_mode_support(0x04);
            av1_ext2.bits.set_obu_size_bytes_minus1(3);
            av1_ext2.bits.set_tile_size_bytes_minus1(3);
        }
        Self {
            profiles: vec![
                VAProfileH264High,
                VAProfileH264Main,
                VAProfileH264ConstrainedBaseline,
                VAProfileHEVCMain,
                VAProfileHEVCMain444,
                VAProfileVP8Version0_3,
                VAProfileVP9Profile0,
                VAProfileVP9Profile1,
                VAProfileAV1Profile0,
            ],
            entrypoints: vec![VAEntrypointEncSliceLP],
            attributes: vec![
                (
                    VAConfigAttribRTFormat,
                    VA_RT_FORMAT_YUV420 | VA_RT_FORMAT_YUV444,
                ),
                (VAConfigAttribRateControl, VA_RC_CBR | VA_RC_CQP),
                (
                    VAConfigAttribEncPackedHeaders,
                    VA_ENC_PACKED_HEADER_SEQUENCE
                        | VA_ENC_PACKED_HEADER_PICTURE
                        | VA_ENC_PACKED_HEADER_SLICE
                        | VA_ENC_PACKED_HEADER_MISC,
                ),
                (VAConfigAttribEncMaxRefFrames, 8 | (1 << 16)),
                (VAConfigAttribEncMaxSlices, 32),
                (
                    VAConfigAttribEncSliceStructure,
                    VA_ENC_SLICE_STRUCTURE_ARBITRARY_ROWS,
                ),
                (VAConfigAttribEncQualityRange, 7),
                (VAConfigAttribEncHEVCFeatures, unsafe {
                    hevc_features.value
                }),
                (VAConfigAttribEncHEVCBlockSizes, unsafe { hevc_block.value }),
                (VAConfigAttribEncAV1Ext2, unsafe { av1_ext2.value }),
            ],
            surface_fourccs: vec![
                VA_FOURCC_NV12,
                VA_FOURCC_444P,
                VA_FOURCC_XYUV,
                VA_FOURCC_BGRA,
                VA_FOURCC_RGBA,
            ],
            derive: true,
            color_standards: vec![
                VAProcColorStandardBT601,
                VAProcColorStandardBT709,
                VAProcColorStandardExplicit,
            ],
            ..Default::default()
        }
    }

    fn id(&mut self) -> u32 {
        self.next_id += 1;
        self.next_id
    }

    /// The buffers of the last picture rendered, of `kind`.
    pub(crate) fn last_buffers(&self, kind: VABufferType) -> Vec<&[u8]> {
        let Some((_, _, indices)) = self.pictures.last() else {
            return Vec::new();
        };
        indices
            .iter()
            .map(|&i| &self.buffers[i])
            .filter(|b| b.1 == kind)
            .map(|b| b.2.as_slice())
            .collect()
    }

    /// The packed headers of the last picture, as `(type, bytes)`.
    pub(crate) fn last_packed(&self) -> Vec<(u32, Vec<u8>)> {
        let Some((_, _, indices)) = self.pictures.last() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut pending = None;
        for &i in indices {
            let (_, kind, bytes) = &self.buffers[i];
            if *kind == VAEncPackedHeaderParameterBufferType {
                pending = Some(u32::from_ne_bytes(bytes[..4].try_into().unwrap()));
            } else if *kind == VAEncPackedHeaderDataBufferType {
                out.push((pending.take().unwrap_or(0), bytes.clone()));
            }
        }
        out
    }

    /// The last picture's parameter buffer of `kind`, read as `T`.
    pub(crate) fn last_param<T: Copy>(&self, kind: VABufferType) -> Option<T> {
        self.last_buffers(kind).first().map(|bytes| {
            assert_eq!(bytes.len(), std::mem::size_of::<T>());
            unsafe { ptr::read_unaligned(bytes.as_ptr() as *const T) }
        })
    }

    /// The coded picture the driver hands back: the packed headers the session wrote, then,
    /// where it wrote no picture or slice header, a frame tag of the kind the picture
    /// parameters ask for.
    fn coded_picture(&self) -> Vec<u8> {
        let packed = self.last_packed();
        let mut coded: Vec<u8> = packed.iter().flat_map(|p| p.1.iter().copied()).collect();
        if packed.iter().all(|p| p.0 == VAEncPackedHeaderSequence) {
            coded.extend(self.frame_tag());
        }
        coded
    }

    /// The bytes a driver writing its own headers codes a picture with, as far as the
    /// session's frame labeling reads them.
    fn frame_tag(&self) -> Vec<u8> {
        let Some((_, _, indices)) = self.pictures.last() else {
            return Vec::new();
        };
        for &i in indices {
            let (_, kind, bytes) = &self.buffers[i];
            if *kind != VAEncPictureParameterBufferType {
                continue;
            }
            if bytes.len() == std::mem::size_of::<VAEncPictureParameterBufferVP9>() {
                let pic: VAEncPictureParameterBufferVP9 =
                    unsafe { ptr::read_unaligned(bytes.as_ptr() as *const _) };
                return vec![
                    if unsafe { pic.pic_flags.bits.frame_type() } == 0 {
                        0x82
                    } else {
                        0x86
                    },
                    0x49,
                    0x83,
                ];
            }
            if bytes.len() == std::mem::size_of::<VAEncPictureParameterBufferVP8>() {
                let pic: VAEncPictureParameterBufferVP8 =
                    unsafe { ptr::read_unaligned(bytes.as_ptr() as *const _) };
                return vec![
                    if unsafe { pic.pic_flags.bits.frame_type() } == 0 {
                        0x10
                    } else {
                        0x11
                    },
                    0,
                    0,
                ];
            }
            if bytes.len() == std::mem::size_of::<VAEncPictureParameterBufferH264>() {
                let pic: VAEncPictureParameterBufferH264 =
                    unsafe { ptr::read_unaligned(bytes.as_ptr() as *const _) };
                return if unsafe { pic.pic_fields.bits.idr_pic_flag() } == 1 {
                    vec![0, 0, 0, 1, 0x65, 0x88]
                } else {
                    vec![0, 0, 0, 1, 0x41, 0x9a]
                };
            }
            if bytes.len() == std::mem::size_of::<VAEncPictureParameterBufferHEVC>() {
                let pic: VAEncPictureParameterBufferHEVC =
                    unsafe { ptr::read_unaligned(bytes.as_ptr() as *const _) };
                return vec![
                    0,
                    0,
                    0,
                    1,
                    if unsafe { pic.pic_fields.bits.idr_pic_flag() } == 1 {
                        19 << 1
                    } else {
                        1 << 1
                    },
                    1,
                    0x80,
                ];
            }
        }
        Vec::new()
    }

    /// The miscellaneous parameters of the last picture, by type.
    pub(crate) fn last_misc(&self) -> Vec<(VAEncMiscParameterType, Vec<u8>)> {
        self.last_buffers(VAEncMiscParameterBufferType)
            .into_iter()
            .map(|b| {
                (
                    u32::from_ne_bytes(b[..4].try_into().unwrap()),
                    b[4..].to_vec(),
                )
            })
            .collect()
    }
}

unsafe extern "C" fn error_str(_status: VAStatus) -> *const c_char {
    c"stood-in driver".as_ptr()
}

unsafe extern "C" fn set_callback(
    _dpy: VADisplay,
    _callback: VAMessageCallback,
    _user: *mut c_void,
) -> VAMessageCallback {
    None
}

unsafe extern "C" fn initialize(_dpy: VADisplay, major: *mut c_int, minor: *mut c_int) -> VAStatus {
    unsafe {
        *major = 1;
        *minor = 24;
    }
    VA_STATUS_SUCCESS as VAStatus
}

unsafe extern "C" fn terminate(_dpy: VADisplay) -> VAStatus {
    with(|d| d.terminated += 1);
    VA_STATUS_SUCCESS as VAStatus
}

unsafe extern "C" fn vendor(_dpy: VADisplay) -> *const c_char {
    with(|d| d.vendor)
        .unwrap_or(c"stood-in driver for pixelflux tests")
        .as_ptr()
}

unsafe extern "C" fn max_profiles(_dpy: VADisplay) -> c_int {
    64
}

unsafe extern "C" fn max_entrypoints(_dpy: VADisplay) -> c_int {
    16
}

unsafe extern "C" fn max_image_formats(_dpy: VADisplay) -> c_int {
    8
}

unsafe extern "C" fn query_profiles(
    _dpy: VADisplay,
    list: *mut VAProfile,
    count: *mut c_int,
) -> VAStatus {
    with(|d| unsafe {
        for (i, &p) in d.profiles.iter().enumerate() {
            *list.add(i) = p;
        }
        *count = d.profiles.len() as c_int;
    });
    VA_STATUS_SUCCESS as VAStatus
}

unsafe extern "C" fn query_entrypoints(
    _dpy: VADisplay,
    profile: VAProfile,
    list: *mut VAEntrypoint,
    count: *mut c_int,
) -> VAStatus {
    with(|d| unsafe {
        let offered: Vec<VAEntrypoint> = if d.profiles.contains(&profile) {
            d.entrypoints.clone()
        } else {
            Vec::new()
        };
        for (i, &e) in offered.iter().enumerate() {
            *list.add(i) = e;
        }
        *count = offered.len() as c_int;
    });
    VA_STATUS_SUCCESS as VAStatus
}

unsafe extern "C" fn get_attributes(
    _dpy: VADisplay,
    _profile: VAProfile,
    entrypoint: VAEntrypoint,
    list: *mut VAConfigAttrib,
    count: c_int,
) -> VAStatus {
    with(|d| unsafe {
        for i in 0..count as usize {
            let attrib = &mut *list.add(i);
            attrib.value = d
                .entrypoint_attributes
                .iter()
                .find(|a| a.0 == entrypoint && a.1 == attrib.type_)
                .map(|a| a.2)
                .or_else(|| {
                    d.attributes
                        .iter()
                        .find(|a| a.0 == attrib.type_)
                        .map(|a| a.1)
                })
                .unwrap_or(VA_ATTRIB_NOT_SUPPORTED);
        }
    });
    VA_STATUS_SUCCESS as VAStatus
}

unsafe extern "C" fn create_config(
    _dpy: VADisplay,
    profile: VAProfile,
    entrypoint: VAEntrypoint,
    list: *mut VAConfigAttrib,
    count: c_int,
    out: *mut VAConfigID,
) -> VAStatus {
    with(|d| unsafe {
        let attribs = (0..count as usize).map(|i| *list.add(i)).collect();
        d.configs.push((profile, entrypoint, attribs));
        *out = d.id();
    });
    VA_STATUS_SUCCESS as VAStatus
}

unsafe extern "C" fn destroy_id(_dpy: VADisplay, _id: u32) -> VAStatus {
    VA_STATUS_SUCCESS as VAStatus
}

unsafe extern "C" fn query_surface_attributes(
    _dpy: VADisplay,
    _config: VAConfigID,
    list: *mut VASurfaceAttrib,
    count: *mut c_uint,
) -> VAStatus {
    with(|d| unsafe {
        if list.is_null() {
            *count = d.surface_fourccs.len() as c_uint;
            return;
        }
        for (i, &fourcc) in d.surface_fourccs.iter().enumerate() {
            let attrib = &mut *list.add(i);
            attrib.type_ = VASurfaceAttribPixelFormat;
            attrib.value.type_ = VAGenericValueTypeInteger;
            attrib.value.value.i = fourcc as i32;
        }
        *count = d.surface_fourccs.len() as c_uint;
    });
    VA_STATUS_SUCCESS as VAStatus
}

unsafe extern "C" fn create_surfaces(
    _dpy: VADisplay,
    rt_format: c_uint,
    width: c_uint,
    height: c_uint,
    out: *mut VASurfaceID,
    count: c_uint,
    list: *mut VASurfaceAttrib,
    attribs: c_uint,
) -> VAStatus {
    with(|d| unsafe {
        let mut fourcc = 0;
        for i in 0..attribs as usize {
            let attrib = &*list.add(i);
            if attrib.type_ == VASurfaceAttribPixelFormat {
                fourcc = attrib.value.value.i as u32;
            }
            if attrib.type_ == VASurfaceAttribExternalBufferDescriptor
                && attrib.value.value.p.is_null()
            {
                return VA_STATUS_ERROR_INVALID_PARAMETER as VAStatus;
            }
        }
        for i in 0..count as usize {
            let id = d.id();
            d.surfaces.push((id, rt_format, width, height));
            *out.add(i) = id;
        }
        let _ = fourcc;
        VA_STATUS_SUCCESS as VAStatus
    })
}

unsafe extern "C" fn destroy_surfaces(
    _dpy: VADisplay,
    list: *mut VASurfaceID,
    count: c_int,
) -> VAStatus {
    with(|d| {
        d.destroyed
            .extend((0..count as usize).map(|i| unsafe { *list.add(i) }))
    });
    VA_STATUS_SUCCESS as VAStatus
}

unsafe extern "C" fn create_context(
    _dpy: VADisplay,
    config: VAConfigID,
    _w: c_int,
    _h: c_int,
    _flag: c_int,
    targets: *mut VASurfaceID,
    count: c_int,
    out: *mut VAContextID,
) -> VAStatus {
    with(|d| unsafe {
        let list = (0..count as usize).map(|i| *targets.add(i)).collect();
        d.contexts.push((config, list));
        *out = d.id();
    });
    VA_STATUS_SUCCESS as VAStatus
}

unsafe extern "C" fn create_buffer(
    _dpy: VADisplay,
    context: VAContextID,
    kind: VABufferType,
    size: c_uint,
    num: c_uint,
    data: *mut c_void,
    out: *mut VABufferID,
) -> VAStatus {
    if kind == VAEncCodedBufferType && !data.is_null() {
        return VA_STATUS_ERROR_INVALID_PARAMETER as VAStatus;
    }
    with(|d| unsafe {
        if kind == VAProcPipelineParameterBufferType && !data.is_null() {
            let p: VAProcPipelineParameterBuffer = ptr::read_unaligned(data as *const _);
            let rect = |r: *const VARectangle| r.as_ref().map(|r| (r.x, r.y, r.width, r.height));
            d.regions
                .push((rect(p.surface_region), rect(p.output_region)));
        }
        let bytes = if data.is_null() {
            vec![0u8; (size * num) as usize]
        } else {
            std::slice::from_raw_parts(data as *const u8, (size * num) as usize).to_vec()
        };
        d.buffers.push((context, kind, bytes));
        *out = d.buffers.len() as u32 - 1 + 0x1000;
    });
    VA_STATUS_SUCCESS as VAStatus
}

unsafe extern "C" fn map_buffer(
    _dpy: VADisplay,
    id: VABufferID,
    out: *mut *mut c_void,
) -> VAStatus {
    with(|d| unsafe {
        let index = (id - 0x1000) as usize;
        let kind = d.buffers[index].1;
        if kind == VAEncCodedBufferType {
            d.coded_out = d.coded.clone().unwrap_or_else(|| d.coded_picture());
            d.segment = VACodedBufferSegment {
                size: d.coded_out.len() as u32,
                bit_offset: 0,
                status: 0,
                reserved: 0,
                buf: d.coded_out.as_mut_ptr() as *mut c_void,
                next: ptr::null_mut(),
                va_reserved: [0; 4],
            };
            *out = &mut d.segment as *mut VACodedBufferSegment as *mut c_void;
        } else {
            *out = d.buffers[index].2.as_mut_ptr() as *mut c_void;
        }
    });
    VA_STATUS_SUCCESS as VAStatus
}

unsafe extern "C" fn unmap_buffer(_dpy: VADisplay, _id: VABufferID) -> VAStatus {
    VA_STATUS_SUCCESS as VAStatus
}

unsafe extern "C" fn begin_picture(
    _dpy: VADisplay,
    context: VAContextID,
    target: VASurfaceID,
) -> VAStatus {
    with(|d| d.in_picture = Some((context, target, Vec::new())));
    VA_STATUS_SUCCESS as VAStatus
}

unsafe extern "C" fn render_picture(
    _dpy: VADisplay,
    _context: VAContextID,
    list: *mut VABufferID,
    count: c_int,
) -> VAStatus {
    with(|d| unsafe {
        let indices: Vec<usize> = (0..count as usize)
            .map(|i| (*list.add(i) - 0x1000) as usize)
            .collect();
        if let Some(p) = &mut d.in_picture {
            p.2.extend(indices);
        }
    });
    VA_STATUS_SUCCESS as VAStatus
}

unsafe extern "C" fn end_picture(_dpy: VADisplay, _context: VAContextID) -> VAStatus {
    with(|d| {
        if let Some(p) = d.in_picture.take() {
            d.pictures.push(p);
        }
    });
    VA_STATUS_SUCCESS as VAStatus
}

unsafe extern "C" fn sync_surface(_dpy: VADisplay, _surface: VASurfaceID) -> VAStatus {
    VA_STATUS_SUCCESS as VAStatus
}

unsafe extern "C" fn query_image_formats(
    _dpy: VADisplay,
    list: *mut VAImageFormat,
    count: *mut c_int,
) -> VAStatus {
    unsafe {
        for (i, fourcc) in [VA_FOURCC_BGRA, VA_FOURCC_RGBA, VA_FOURCC_NV12]
            .into_iter()
            .enumerate()
        {
            let f = &mut *list.add(i);
            *f = std::mem::zeroed();
            f.fourcc = fourcc;
            f.bits_per_pixel = 32;
        }
        *count = 3;
    }
    VA_STATUS_SUCCESS as VAStatus
}

/// An image over one of the driver's own byte buffers, four bytes a pixel.
fn image(width: c_int, height: c_int, fourcc: u32) -> VAImage {
    let bytes = vec![0u8; (width * height * 4) as usize];
    let (id, buf) = with(|d| {
        d.buffers.push((0, VAImageBufferType, bytes));
        let buf = d.buffers.len() as u32 - 1 + 0x1000;
        d.images.push(buf);
        (d.id(), buf)
    });
    let mut image: VAImage = unsafe { std::mem::zeroed() };
    image.image_id = id;
    image.format.fourcc = fourcc;
    image.buf = buf;
    image.width = width as u16;
    image.height = height as u16;
    image.num_planes = 1;
    image.pitches[0] = (width * 4) as u32;
    image
}

unsafe extern "C" fn create_image(
    _dpy: VADisplay,
    format: *mut VAImageFormat,
    width: c_int,
    height: c_int,
    out: *mut VAImage,
) -> VAStatus {
    if with(|d| d.image_fails) {
        return VA_STATUS_ERROR_OPERATION_FAILED as VAStatus;
    }
    unsafe { *out = image(width, height, (*format).fourcc) };
    VA_STATUS_SUCCESS as VAStatus
}

unsafe extern "C" fn derive_image(
    _dpy: VADisplay,
    surface: VASurfaceID,
    out: *mut VAImage,
) -> VAStatus {
    let (derive, size) = with(|d| {
        (
            d.derive,
            d.surfaces
                .iter()
                .find(|s| s.0 == surface)
                .map(|s| (s.2, s.3)),
        )
    });
    let Some((w, h)) = size else {
        return VA_STATUS_ERROR_INVALID_PARAMETER as VAStatus;
    };
    if !derive {
        return VA_STATUS_ERROR_OPERATION_FAILED as VAStatus;
    }
    unsafe { *out = image(w as c_int, h as c_int, VA_FOURCC_BGRA) };
    VA_STATUS_SUCCESS as VAStatus
}

#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn put_image(
    _dpy: VADisplay,
    _surface: VASurfaceID,
    _image: VAImageID,
    _sx: c_int,
    _sy: c_int,
    _sw: c_uint,
    _sh: c_uint,
    _dx: c_int,
    _dy: c_int,
    _dw: c_uint,
    _dh: c_uint,
) -> VAStatus {
    with(|d| d.puts += 1);
    VA_STATUS_SUCCESS as VAStatus
}

unsafe extern "C" fn pipeline_caps(
    _dpy: VADisplay,
    _context: VAContextID,
    _filters: *mut VABufferID,
    _count: c_uint,
    out: *mut VAProcPipelineCaps,
) -> VAStatus {
    thread_local! {
        static STANDARDS: RefCell<Vec<VAProcColorStandardType>> = const { RefCell::new(Vec::new()) };
    }
    STANDARDS.with(|s| {
        *s.borrow_mut() = with(|d| d.color_standards.clone());
        let mut list = s.borrow_mut();
        unsafe {
            let caps = &mut *out;
            *caps = std::mem::zeroed();
            caps.input_color_standards = list.as_mut_ptr();
            caps.num_input_color_standards = list.len() as u32;
            caps.output_color_standards = list.as_mut_ptr();
            caps.num_output_color_standards = list.len() as u32;
        }
    });
    VA_STATUS_SUCCESS as VAStatus
}

unsafe extern "C" fn get_display(_fd: c_int) -> VADisplay {
    0x5a as VADisplay
}

/// The stood-in libva table.
pub(crate) fn api() -> VaApi {
    VaApi {
        vaErrorStr: error_str,
        vaSetErrorCallback: set_callback,
        vaSetInfoCallback: set_callback,
        vaInitialize: initialize,
        vaTerminate: terminate,
        vaQueryVendorString: vendor,
        vaMaxNumProfiles: max_profiles,
        vaMaxNumEntrypoints: max_entrypoints,
        vaMaxNumImageFormats: max_image_formats,
        vaQueryConfigProfiles: query_profiles,
        vaQueryConfigEntrypoints: query_entrypoints,
        vaGetConfigAttributes: get_attributes,
        vaCreateConfig: create_config,
        vaDestroyConfig: destroy_id,
        vaQuerySurfaceAttributes: query_surface_attributes,
        vaCreateSurfaces: create_surfaces,
        vaDestroySurfaces: destroy_surfaces,
        vaCreateContext: create_context,
        vaDestroyContext: destroy_id,
        vaCreateBuffer: create_buffer,
        vaMapBuffer: map_buffer,
        vaUnmapBuffer: unmap_buffer,
        vaDestroyBuffer: destroy_id,
        vaBeginPicture: begin_picture,
        vaRenderPicture: render_picture,
        vaEndPicture: end_picture,
        vaSyncSurface: sync_surface,
        vaSyncBuffer: None,
        vaQueryImageFormats: query_image_formats,
        vaCreateImage: create_image,
        vaDestroyImage: destroy_id,
        vaPutImage: put_image,
        vaDeriveImage: derive_image,
        vaQueryVideoProcPipelineCaps: pipeline_caps,
        vaGetDisplayDRM: get_display,
    }
}
