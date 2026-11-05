/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Decoders for the client camera uplink, and for the checks that read a session's own output
//! back.
//!
//! Clients send whatever their browser can produce: H.264 (WebCodecs or the WebRTC media track),
//! VP8/VP9/AV1/HEVC (same sources), or MJPEG from the canvas fallback. Every codec lands in the
//! same I420 view so the rest of the pipeline is codec-agnostic. H.264 decodes through Cisco
//! OpenH264, VP8 and VP9 through libvpx, AV1 through dav1d, HEVC through libde265, and MJPEG
//! through TurboJPEG, which pixelflux already uses for its own JPEG stripes. Decoding is
//! software here; a hardware decoder only changes which `Decoder` is constructed.

use std::ffi::{CStr, c_int, c_void};
use std::ptr;

use turbojpeg::{Decompressor, Image, PixelFormat, Subsamp, YuvImage};

use super::convert::{I420Buffer, I420View};

/// Input codecs, by wire id. The ids are part of the Selkies WebSocket framing and are exported to
/// Python as `VirtualCamera.CODEC_*`.
pub use crate::encoders::codec::Codec;
use crate::encoders::codec::{
    FRAME_KEY, annexb_nals, av1_is_key, h264_frame_type, h265_frame_type, vp8_is_key, vp9_is_key,
};
use crate::encoders::sps;

/// Whether an encoded frame is a key frame, read from the bitstream itself: every codec here
/// declares it in its first bytes or NAL types.
pub fn sniff_keyframe(codec: Codec, data: &[u8]) -> Option<bool> {
    Some(match codec {
        Codec::Jpeg => true,
        Codec::H264 => h264_frame_type(data) == FRAME_KEY,
        Codec::H265 => h265_frame_type(data) == FRAME_KEY,
        Codec::Vp8 => vp8_is_key(data),
        Codec::Vp9 => vp9_is_key(data),
        Codec::Av1 => av1_is_key(data),
    })
}

#[derive(Debug)]
pub enum DecodeError {
    /// The packet could not be decoded; the stream needs a keyframe to resynchronize.
    Corrupt(String),
    /// The decoder itself failed and must be recreated.
    Fatal(String),
}

pub trait Decoder {
    fn codec(&self) -> Codec;
    /// Decode one encoded frame. `Ok(true)` when `frame()` now holds a new picture.
    fn decode(&mut self, data: &[u8]) -> Result<bool, DecodeError>;
    fn frame(&self) -> Option<I420View<'_>>;
}

pub fn new_decoder(codec: Codec) -> Result<Box<dyn Decoder>, String> {
    match codec {
        Codec::Jpeg => Ok(Box::new(JpegDecoder::new()?)),
        _ => Ok(Box::new(VideoDecoder::new(codec)?)),
    }
}

/// The color a decoded stream declares: the matrix as ISO/IEC 23091-2 codes it (1 BT.709,
/// 5 BT.470BG, 6 SMPTE 170M, 2 unspecified) and whether the range is full.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColorTags {
    pub matrix: u8,
    pub full_range: bool,
}

impl ColorTags {
    pub const BT709_LIMITED: Self = Self {
        matrix: 1,
        full_range: false,
    };
    pub const BT709_FULL: Self = Self {
        matrix: 1,
        full_range: true,
    };
    pub const BT470BG_LIMITED: Self = Self {
        matrix: 5,
        full_range: false,
    };
    pub const BT470BG_FULL: Self = Self {
        matrix: 5,
        full_range: true,
    };
}

/// The chroma sampling of a decoded picture.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Chroma {
    C420,
    C422,
    C444,
}

/// One decoded picture as a backend hands it back: borrowed planes, valid until the backend
/// decodes again.
struct Picture {
    width: usize,
    height: usize,
    chroma: Chroma,
    planes: [*const u8; 3],
    strides: [usize; 3],
    tags: Option<ColorTags>,
}

impl Picture {
    unsafe fn plane(&self, i: usize, rows: usize, cols: usize) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.planes[i], self.strides[i] * (rows - 1) + cols) }
    }
}

/// A software decoder for one of the inter-coded codecs.
pub struct VideoDecoder {
    codec: Codec,
    backend: Backend,
    picture: Option<Picture>,
    /// Planar copy of pictures that are not 4:2:0, or that a backend does not keep for us.
    converted: I420Buffer,
    from_converted: bool,
}

unsafe impl Send for VideoDecoder {}

enum Backend {
    H264(H264Backend),
    Vpx(VpxBackend),
    Av1(Box<Dav1dBackend>),
    Hevc(De265Backend),
}

impl VideoDecoder {
    pub fn new(codec: Codec) -> Result<Self, String> {
        let backend = match codec {
            Codec::H264 => Backend::H264(H264Backend::new()?),
            Codec::Vp8 | Codec::Vp9 => Backend::Vpx(VpxBackend::new(codec)?),
            Codec::Av1 => Backend::Av1(Box::new(Dav1dBackend::new()?)),
            Codec::H265 => Backend::Hevc(De265Backend::new()?),
            Codec::Jpeg => return Err("MJPEG decodes through the JPEG decoder".into()),
        };
        Ok(Self {
            codec,
            backend,
            picture: None,
            converted: I420Buffer::new(2, 2),
            from_converted: false,
        })
    }

    /// The color matrix and range the last decoded frame carries, as the bitstream declared
    /// them; unspecified where the codec cannot declare a matrix.
    pub fn color_tags(&self) -> Option<ColorTags> {
        self.picture.as_ref()?.tags
    }

    /// Average a 4:2:2 or 4:4:4 picture's chroma into the I420 copy.
    fn convert(&mut self) {
        let Some(p) = &self.picture else { return };
        let (w, h) = (p.width, p.height);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        self.converted.resize(w, h);
        let y_len = self.converted.y_len();
        let uv_len = self.converted.uv_len();
        let (yp, rest) = self.converted.data.split_at_mut(y_len);
        let (up, vp) = rest.split_at_mut(uv_len);
        let y = unsafe { p.plane(0, h, w) };
        for row in 0..h {
            yp[row * w..row * w + w]
                .copy_from_slice(&y[row * p.strides[0]..row * p.strides[0] + w]);
        }
        let (src_cw, src_ch) = match p.chroma {
            Chroma::C420 => (cw, ch),
            Chroma::C422 => (cw, h),
            Chroma::C444 => (w, h),
        };
        for (i, dst) in [(1usize, &mut *up), (2usize, &mut *vp)] {
            let src = unsafe { p.plane(i, src_ch, src_cw) };
            let s = p.strides[i];
            for row in 0..ch {
                for x in 0..cw {
                    let sum: u32 = match p.chroma {
                        Chroma::C420 => src[row * s + x] as u32 * 4,
                        Chroma::C422 => {
                            let (r0, r1) =
                                ((2 * row).min(src_ch - 1), (2 * row + 1).min(src_ch - 1));
                            (src[r0 * s + x] as u32 + src[r1 * s + x] as u32) * 2
                        }
                        Chroma::C444 => {
                            let (r0, r1) =
                                ((2 * row).min(src_ch - 1), (2 * row + 1).min(src_ch - 1));
                            let (x0, x1) = (2 * x, (2 * x + 1).min(src_cw - 1));
                            src[r0 * s + x0] as u32
                                + src[r0 * s + x1] as u32
                                + src[r1 * s + x0] as u32
                                + src[r1 * s + x1] as u32
                        }
                    };
                    dst[row * cw + x] = ((sum + 2) / 4) as u8;
                }
            }
        }
    }
}

impl Decoder for VideoDecoder {
    fn codec(&self) -> Codec {
        self.codec
    }

    fn decode(&mut self, data: &[u8]) -> Result<bool, DecodeError> {
        self.picture = None;
        let picture = match &mut self.backend {
            Backend::H264(b) => b.decode(data)?,
            Backend::Vpx(b) => b.decode(data)?,
            Backend::Av1(b) => b.decode(data)?,
            Backend::Hevc(b) => b.decode(data)?,
        };
        let Some(picture) = picture else {
            return Ok(false);
        };
        self.from_converted = picture.chroma != Chroma::C420;
        self.picture = Some(picture);
        if self.from_converted {
            self.convert();
        }
        Ok(true)
    }

    fn frame(&self) -> Option<I420View<'_>> {
        let p = self.picture.as_ref()?;
        let full_range = p.tags.is_some_and(|t| t.full_range);
        if self.from_converted {
            return Some(self.converted.view(full_range));
        }
        let (w, h) = (p.width, p.height);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        Some(I420View {
            width: w,
            height: h,
            y: unsafe { p.plane(0, h, w) },
            u: unsafe { p.plane(1, ch, cw) },
            v: unsafe { p.plane(2, ch, cw) },
            y_stride: p.strides[0],
            uv_stride: p.strides[1].min(p.strides[2]),
            full_range,
        })
    }
}

/// Cisco OpenH264, driven through its decoder interface: it conceals the blocks a stream
/// lost and hands back every picture it decodes, one behind a gap included, and the picture
/// stays in its own buffer until the next call.
struct H264Backend {
    decoder: *mut openh264_sys2::ISVCDecoder,
    tags: Option<ColorTags>,
}

impl Drop for H264Backend {
    fn drop(&mut self) {
        unsafe {
            if let Some(uninit) = self.api().and_then(|a| a.Uninitialize) {
                uninit(self.decoder);
            }
            openh264_sys2::source::APILoader::WelsDestroyDecoder(self.decoder);
        }
    }
}

impl H264Backend {
    /// The decoder's method table, behind the two pointers OpenH264 hands out.
    fn api(&self) -> Option<&openh264_sys2::ISVCDecoderVtbl> {
        unsafe { self.decoder.as_ref().and_then(|d| d.as_ref()) }
    }

    fn new() -> Result<Self, String> {
        use openh264_sys2::*;
        let mut decoder: *mut ISVCDecoder = ptr::null_mut();
        if unsafe { source::APILoader::WelsCreateDecoder(&mut decoder) } != 0 {
            return Err("OpenH264 created no decoder".into());
        }
        let me = Self {
            decoder,
            tags: None,
        };
        let initialize = me
            .api()
            .and_then(|a| a.Initialize)
            .ok_or("OpenH264 exposes no Initialize")?;
        let mut param: SDecodingParam = unsafe { std::mem::zeroed() };
        param.eEcActiveIdc = ERROR_CON_SLICE_MV_COPY_CROSS_IDR;
        param.sVideoProperty.size = std::mem::size_of::<SVideoProperty>() as u32;
        param.sVideoProperty.eVideoBsType = VIDEO_BITSTREAM_AVC;
        if unsafe { initialize(decoder, &param) } != 0 {
            return Err("OpenH264 refused to initialize".into());
        }
        Ok(me)
    }

    fn decode(&mut self, data: &[u8]) -> Result<Option<Picture>, DecodeError> {
        use openh264_sys2::*;
        let mut sps = None;
        let mut scalable = false;
        for nal in annexb_nals(data) {
            match nal[0] & 0x1f {
                7 if sps.is_none() => sps = Some(nal),
                14 | 15 | 20 => scalable = true,
                _ => {}
            }
        }
        if let Some(sps) = sps {
            self.tags = Some(match sps::read_color(sps) {
                Some(signal) => ColorTags {
                    matrix: signal.matrix,
                    full_range: signal.full_range,
                },
                None => ColorTags {
                    matrix: 2,
                    full_range: false,
                },
            });
        }
        // OpenH264 also decodes the scalable extension, and a prefix or extension NAL unit naming
        // a layer above the base turns it to that layer for good, so it hands over no picture
        // again, key frames included. An AVC decoder ignores the extension's units (prefix,
        // subset SPS, extension slice), and so does this one.
        let mut avc = Vec::new();
        let data = if scalable {
            for nal in annexb_nals(data).filter(|n| !matches!(n[0] & 0x1f, 14 | 15 | 20)) {
                avc.extend_from_slice(&[0, 0, 0, 1]);
                avc.extend_from_slice(nal);
            }
            &avc[..]
        } else {
            data
        };
        let mut dst: [*mut u8; 3] = [ptr::null_mut(); 3];
        let mut info: SBufferInfo = unsafe { std::mem::zeroed() };
        let decode = self
            .api()
            .and_then(|a| a.DecodeFrameNoDelay)
            .ok_or_else(|| DecodeError::Fatal("OpenH264 exposes no DecodeFrameNoDelay".into()))?;
        let mut state = unsafe {
            decode(
                self.decoder,
                data.as_ptr(),
                data.len() as c_int,
                dst.as_mut_ptr(),
                &mut info,
            )
        };
        // A profile above baseline has the decoder hold each picture back until the next
        // one arrives, against reordering the stream never does: the held picture is this
        // access unit's, and flushing hands it over now. OpenH264 2.6 keeps a flushed
        // picture's buffer when decoding single-threaded, so such a stream stalls once the
        // pool runs dry; a Baseline stream is never held.
        if info.iBufferStatus != 1
            && let Some(flush) = self.api().and_then(|a| a.FlushFrame)
        {
            state |= unsafe { flush(self.decoder, dst.as_mut_ptr(), &mut info) };
        }
        if info.iBufferStatus != 1 || dst.iter().any(|p| p.is_null()) {
            if state != dsErrorFree {
                return Err(DecodeError::Corrupt(format!(
                    "OpenH264 decoding state {state:#x}"
                )));
            }
            return Ok(None);
        }
        let system = unsafe { info.UsrData.sSystemBuffer };
        Ok(Some(Picture {
            width: system.iWidth as usize,
            height: system.iHeight as usize,
            chroma: Chroma::C420,
            planes: [dst[0], dst[1], dst[2]],
            strides: [
                system.iStride[0] as usize,
                system.iStride[1] as usize,
                system.iStride[1] as usize,
            ],
            tags: self.tags,
        }))
    }
}

/// libvpx's VP8 and VP9 decoders. VP9 declares its matrix and range in every frame; VP8's
/// keyframe header carries one color-space bit, BT.601 or reserved, and a clamping bit.
struct VpxBackend {
    codec: Codec,
    ctx: codec_sys::vpx::vpx_codec_ctx_t,
    tags: Option<ColorTags>,
}

impl Drop for VpxBackend {
    fn drop(&mut self) {
        unsafe { codec_sys::vpx::vpx_codec_destroy(&mut self.ctx) };
    }
}

/// VP8's `color_space` and `clamping_type` bits, the first two bool-coded fields of a key
/// frame's first partition.
fn vp8_color_bits(frame: &[u8]) -> Option<(bool, bool)> {
    if !vp8_is_key(frame) || frame.len() < 12 {
        return None;
    }
    let data = &frame[10..];
    let mut value = ((data[0] as u32) << 8) | data[1] as u32;
    let mut range = 255u32;
    let mut bit_count = 0;
    let mut at = 2;
    let mut read = || {
        let split = 1 + (((range - 1) * 128) >> 8);
        let big = split << 8;
        let bit = value >= big;
        if bit {
            range -= split;
            value -= big;
        } else {
            range = split;
        }
        while range < 128 {
            value <<= 1;
            range <<= 1;
            bit_count += 1;
            if bit_count == 8 {
                bit_count = 0;
                value |= data.get(at).copied().unwrap_or(0) as u32;
                at += 1;
            }
        }
        bit
    };
    let color_space = read();
    let clamping = read();
    Some((color_space, clamping))
}

impl VpxBackend {
    fn new(codec: Codec) -> Result<Self, String> {
        use codec_sys::vpx::*;
        let iface = unsafe {
            if codec == Codec::Vp8 {
                vpx_codec_vp8_dx()
            } else {
                vpx_codec_vp9_dx()
            }
        };
        let mut ctx: vpx_codec_ctx_t = unsafe { std::mem::zeroed() };
        if unsafe {
            vpx_codec_dec_init_ver(
                &mut ctx,
                iface,
                ptr::null(),
                0,
                VPX_DECODER_ABI_VERSION as c_int,
            )
        } != VPX_CODEC_OK
        {
            return Err(format!("libvpx opened no {} decoder", codec.name()));
        }
        Ok(Self {
            codec,
            ctx,
            tags: None,
        })
    }

    fn decode(&mut self, data: &[u8]) -> Result<Option<Picture>, DecodeError> {
        use codec_sys::vpx::*;
        if self.codec == Codec::Vp8
            && let Some((reserved, clamping)) = vp8_color_bits(data)
        {
            self.tags = Some(ColorTags {
                matrix: if reserved { 2 } else { 5 },
                full_range: clamping,
            });
        }
        let res = unsafe {
            vpx_codec_decode(
                &mut self.ctx,
                data.as_ptr(),
                data.len() as u32,
                ptr::null_mut(),
                0,
            )
        };
        if res != VPX_CODEC_OK {
            let detail = unsafe { CStr::from_ptr(vpx_codec_err_to_string(res)) }
                .to_string_lossy()
                .into_owned();
            return Err(DecodeError::Corrupt(detail));
        }
        let mut iter: vpx_codec_iter_t = ptr::null_mut();
        let mut image = None;
        while let Some(next) =
            ptr::NonNull::new(unsafe { vpx_codec_get_frame(&mut self.ctx, &mut iter) })
        {
            image = Some(next);
        }
        let Some(image) = image else { return Ok(None) };
        let img = unsafe { image.as_ref() };
        if img.bit_depth != 8 {
            return Err(DecodeError::Fatal(format!(
                "unsupported libvpx bit depth {}",
                img.bit_depth
            )));
        }
        let chroma = match (img.x_chroma_shift, img.y_chroma_shift) {
            (1, 1) => Chroma::C420,
            (1, 0) => Chroma::C422,
            (0, 0) => Chroma::C444,
            _ => {
                return Err(DecodeError::Fatal(format!(
                    "unsupported libvpx image format {}",
                    img.fmt
                )));
            }
        };
        if self.codec == Codec::Vp9 {
            let matrix = match img.cs {
                VPX_CS_BT_601 => 5,
                VPX_CS_BT_709 => 1,
                VPX_CS_SMPTE_170 => 6,
                VPX_CS_SMPTE_240 => 7,
                VPX_CS_BT_2020 => 9,
                VPX_CS_SRGB => 0,
                _ => 2,
            };
            self.tags = Some(ColorTags {
                matrix,
                full_range: img.range == VPX_CR_FULL_RANGE,
            });
        }
        Ok(Some(Picture {
            width: img.d_w as usize,
            height: img.d_h as usize,
            chroma,
            planes: [img.planes[0], img.planes[1], img.planes[2]],
            strides: [
                img.stride[0] as usize,
                img.stride[1] as usize,
                img.stride[2] as usize,
            ],
            tags: self.tags,
        }))
    }
}

/// dav1d, at the frame delay of one a live uplink wants.
struct Dav1dBackend {
    ctx: *mut codec_sys::dav1d::Dav1dContext,
    picture: codec_sys::dav1d::Dav1dPicture,
    held: bool,
}

impl Drop for Dav1dBackend {
    fn drop(&mut self) {
        unsafe {
            if self.held {
                codec_sys::dav1d::dav1d_picture_unref(&mut self.picture);
            }
            codec_sys::dav1d::dav1d_close(&mut self.ctx);
        }
    }
}

impl Dav1dBackend {
    fn new() -> Result<Self, String> {
        use codec_sys::dav1d::*;
        let mut settings: Dav1dSettings = unsafe { std::mem::zeroed() };
        unsafe { dav1d_default_settings(&mut settings) };
        settings.n_threads = 2;
        settings.max_frame_delay = 1;
        settings.logger.callback = None;
        let mut ctx: *mut Dav1dContext = ptr::null_mut();
        if unsafe { dav1d_open(&mut ctx, &settings) } != 0 || ctx.is_null() {
            return Err("dav1d opened no decoder".into());
        }
        Ok(Self {
            ctx,
            picture: unsafe { std::mem::zeroed() },
            held: false,
        })
    }

    fn decode(&mut self, data: &[u8]) -> Result<Option<Picture>, DecodeError> {
        use codec_sys::dav1d::*;
        const AGAIN: c_int = -(libc::EAGAIN);
        let mut input: Dav1dData = unsafe { std::mem::zeroed() };
        let buffer = unsafe { dav1d_data_create(&mut input, data.len()) };
        if buffer.is_null() {
            return Err(DecodeError::Fatal("dav1d allocated no input buffer".into()));
        }
        unsafe { ptr::copy_nonoverlapping(data.as_ptr(), buffer, data.len()) };
        let mut got = false;
        loop {
            let sent = unsafe { dav1d_send_data(self.ctx, &mut input) };
            if sent < 0 && sent != AGAIN {
                unsafe { dav1d_data_unref(&mut input) };
                return Err(DecodeError::Corrupt(format!(
                    "dav1d refused the data ({sent})"
                )));
            }
            loop {
                let mut next: Dav1dPicture = unsafe { std::mem::zeroed() };
                let res = unsafe { dav1d_get_picture(self.ctx, &mut next) };
                if res == AGAIN {
                    break;
                }
                if res < 0 {
                    unsafe { dav1d_data_unref(&mut input) };
                    return Err(DecodeError::Corrupt(format!(
                        "dav1d decoded no picture ({res})"
                    )));
                }
                if self.held {
                    unsafe { dav1d_picture_unref(&mut self.picture) };
                }
                self.picture = next;
                self.held = true;
                got = true;
            }
            if sent == 0 {
                break;
            }
        }
        unsafe { dav1d_data_unref(&mut input) };
        if !got {
            return Ok(None);
        }
        let p = &self.picture;
        let chroma = match p.p.layout {
            DAV1D_PIXEL_LAYOUT_I420 => Chroma::C420,
            DAV1D_PIXEL_LAYOUT_I422 => Chroma::C422,
            DAV1D_PIXEL_LAYOUT_I444 => Chroma::C444,
            other => {
                return Err(DecodeError::Fatal(format!(
                    "unsupported dav1d layout {other}"
                )));
            }
        };
        if p.p.bpc != 8 {
            return Err(DecodeError::Fatal(format!(
                "unsupported dav1d bit depth {}",
                p.p.bpc
            )));
        }
        let tags = unsafe { p.seq_hdr.as_ref() }.map(|s| ColorTags {
            matrix: s.mtrx as u8,
            full_range: s.color_range != 0,
        });
        Ok(Some(Picture {
            width: p.p.w as usize,
            height: p.p.h as usize,
            chroma,
            planes: [
                p.data[0] as *const u8,
                p.data[1] as *const u8,
                p.data[2] as *const u8,
            ],
            strides: [
                p.stride[0] as usize,
                p.stride[1] as usize,
                p.stride[1] as usize,
            ],
            tags,
        }))
    }
}

/// libde265. Its picture is valid only until the next call into the library, so the planes are
/// copied out as they come.
struct De265Backend {
    ctx: *mut codec_sys::de265::de265_decoder_context,
    planes: Vec<u8>,
}

impl Drop for De265Backend {
    fn drop(&mut self) {
        unsafe { codec_sys::de265::de265_free_decoder(self.ctx) };
    }
}

impl De265Backend {
    fn new() -> Result<Self, String> {
        use codec_sys::de265::*;
        let ctx = unsafe { de265_new_decoder() };
        if ctx.is_null() {
            return Err("libde265 opened no decoder".into());
        }
        unsafe {
            de265_set_parameter_bool(ctx, DE265_DECODER_PARAM_BOOL_SEI_CHECK_HASH, 0);
            de265_start_worker_threads(ctx, 2);
        }
        Ok(Self {
            ctx,
            planes: Vec::new(),
        })
    }

    fn decode(&mut self, data: &[u8]) -> Result<Option<Picture>, DecodeError> {
        use codec_sys::de265::*;
        let text = |err: de265_error| {
            unsafe { CStr::from_ptr(de265_get_error_text(err)) }
                .to_string_lossy()
                .into_owned()
        };
        unsafe {
            let err = de265_push_data(
                self.ctx,
                data.as_ptr() as *const c_void,
                data.len() as c_int,
                0,
                ptr::null_mut(),
            );
            if err != DE265_OK {
                return Err(DecodeError::Corrupt(text(err)));
            }
            de265_push_end_of_frame(self.ctx);
            // A decoded picture sits in the output queue until it is taken, and the decoder
            // reports more to do for as long as one does, so each round takes what is there.
            let mut image: *const de265_image = ptr::null();
            let mut more: c_int = 1;
            while more != 0 && image.is_null() {
                let err = de265_decode(self.ctx, &mut more);
                if err != DE265_OK && err != DE265_ERROR_WAITING_FOR_INPUT_DATA {
                    return Err(DecodeError::Corrupt(text(err)));
                }
                loop {
                    let next = de265_get_next_picture(self.ctx);
                    if next.is_null() {
                        break;
                    }
                    image = next;
                }
                if err == DE265_ERROR_WAITING_FOR_INPUT_DATA {
                    break;
                }
            }
            if image.is_null() {
                return Ok(None);
            }
            let (w, h) = (
                de265_get_image_width(image, 0) as usize,
                de265_get_image_height(image, 0) as usize,
            );
            let format = de265_get_chroma_format(image);
            let chroma = if format == de265_chroma_420 {
                Chroma::C420
            } else if format == de265_chroma_422 {
                Chroma::C422
            } else if format == de265_chroma_444 {
                Chroma::C444
            } else {
                return Err(DecodeError::Fatal(format!(
                    "unsupported libde265 chroma format {format}"
                )));
            };
            if (0..3).any(|c| de265_get_bits_per_pixel(image, c) != 8) {
                return Err(DecodeError::Fatal("unsupported libde265 bit depth".into()));
            }
            let tags = Some(ColorTags {
                matrix: de265_get_image_matrix_coefficients(image) as u8,
                full_range: de265_get_image_full_range_flag(image) != 0,
            });
            // The image lives only until the next libde265 call, so its planes are copied
            // into this backend's own buffer, laid out as the picture was.
            let (cw, ch) = match chroma {
                Chroma::C420 => (w.div_ceil(2), h.div_ceil(2)),
                Chroma::C422 => (w.div_ceil(2), h),
                Chroma::C444 => (w, h),
            };
            self.planes.resize(w * h + 2 * cw * ch, 0);
            let (yd, rest) = self.planes.split_at_mut(w * h);
            let (ud, vd) = rest.split_at_mut(cw * ch);
            for (c, dst, cols, rows) in [(0, yd, w, h), (1, ud, cw, ch), (2, vd, cw, ch)] {
                let mut stride: c_int = 0;
                let src = de265_get_image_plane(image, c, &mut stride);
                for row in 0..rows {
                    ptr::copy_nonoverlapping(
                        src.add(row * stride as usize),
                        dst.as_mut_ptr().add(row * cols),
                        cols,
                    );
                }
            }
            Ok(Some(Picture {
                width: w,
                height: h,
                chroma,
                planes: [
                    self.planes.as_ptr(),
                    self.planes[w * h..].as_ptr(),
                    self.planes[w * h + cw * ch..].as_ptr(),
                ],
                strides: [w, cw, cw],
                tags,
            }))
        }
    }
}

/// TurboJPEG decoder: 4:2:0 JPEGs decode straight into I420 planes; other subsamplings take the
/// RGB route and are converted.
pub struct JpegDecoder {
    dec: Decompressor,
    out: I420Buffer,
    rgb: Vec<u8>,
    have_frame: bool,
}

impl JpegDecoder {
    pub fn new() -> Result<Self, String> {
        Ok(JpegDecoder {
            dec: Decompressor::new().map_err(|e| format!("turbojpeg: {}", e))?,
            out: I420Buffer::new(2, 2),
            rgb: Vec::new(),
            have_frame: false,
        })
    }
}

/// The widest or tallest picture a camera's JPEG may claim, far past any webcam.
const MAX_JPEG_SIDE: usize = 8192;

impl Decoder for JpegDecoder {
    fn codec(&self) -> Codec {
        Codec::Jpeg
    }

    fn decode(&mut self, data: &[u8]) -> Result<bool, DecodeError> {
        self.have_frame = false;
        let hdr = self
            .dec
            .read_header(data)
            .map_err(|e| DecodeError::Corrupt(format!("jpeg header: {}", e)))?;
        if hdr.width == 0 || hdr.height == 0 {
            return Err(DecodeError::Corrupt("empty jpeg".into()));
        }
        // The header is the peer's word, and the buffers are sized from it before any
        // pixel decodes.
        if hdr.width > MAX_JPEG_SIDE || hdr.height > MAX_JPEG_SIDE {
            return Err(DecodeError::Corrupt(format!(
                "jpeg of {}x{}",
                hdr.width, hdr.height
            )));
        }
        self.out.resize(hdr.width, hdr.height);
        if hdr.subsamp == Subsamp::Sub2x2 {
            let img = YuvImage {
                pixels: &mut self.out.data[..],
                width: hdr.width,
                align: 1,
                height: hdr.height,
                subsamp: Subsamp::Sub2x2,
            };
            self.dec
                .decompress_to_yuv(data, img)
                .map_err(|e| DecodeError::Corrupt(format!("jpeg: {}", e)))?;
        } else {
            self.rgb.resize(hdr.width * hdr.height * 4, 0);
            let img = Image {
                pixels: &mut self.rgb[..],
                width: hdr.width,
                pitch: hdr.width * 4,
                height: hdr.height,
                format: PixelFormat::RGBA,
            };
            self.dec
                .decompress(data, img)
                .map_err(|e| DecodeError::Corrupt(format!("jpeg: {}", e)))?;
            let y_len = self.out.y_len();
            let uv_len = self.out.uv_len();
            let cw = hdr.width.div_ceil(2);
            let (yp, rest) = self.out.data.split_at_mut(y_len);
            let (up, vp) = rest.split_at_mut(uv_len);
            let mut planar = yuv::YuvPlanarImageMut {
                y_plane: yuv::BufferStoreMut::Borrowed(yp),
                y_stride: hdr.width as u32,
                u_plane: yuv::BufferStoreMut::Borrowed(up),
                u_stride: cw as u32,
                v_plane: yuv::BufferStoreMut::Borrowed(vp),
                v_stride: cw as u32,
                width: hdr.width as u32,
                height: hdr.height as u32,
            };
            yuv::rgba_to_yuv420(
                &mut planar,
                &self.rgb,
                (hdr.width * 4) as u32,
                yuv::YuvRange::Full,
                yuv::YuvStandardMatrix::Bt601,
                yuv::YuvConversionMode::Fast,
            )
            .map_err(|e| DecodeError::Corrupt(format!("rgb->i420: {:?}", e)))?;
        }
        self.have_frame = true;
        Ok(true)
    }

    fn frame(&self) -> Option<I420View<'_>> {
        if self.have_frame {
            Some(self.out.view(true))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_ids_round_trip() {
        for id in 0..6 {
            let c = Codec::from_id(id).unwrap();
            assert_eq!(c as u32, id);
        }
        assert!(Codec::from_id(6).is_none());
    }

    #[test]
    fn h264_keyframe_sniff() {
        let idr = [
            0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xCE, 0, 0, 1, 0x65, 0x88,
        ];
        assert_eq!(sniff_keyframe(Codec::H264, &idr), Some(true));
        let p = [0, 0, 0, 1, 0x41, 0x9A];
        assert_eq!(sniff_keyframe(Codec::H264, &p), Some(false));
        assert_eq!(sniff_keyframe(Codec::Vp8, &[0x10, 0, 0]), Some(true));
        assert_eq!(sniff_keyframe(Codec::Vp8, &[0x11, 0, 0]), Some(false));
        assert_eq!(sniff_keyframe(Codec::Vp9, &[0x82, 0x49, 0x83]), Some(true));
        assert_eq!(sniff_keyframe(Codec::Vp9, &[0x86, 0]), Some(false));
        assert_eq!(
            sniff_keyframe(Codec::H265, &[0, 0, 1, 0x26, 0x01]),
            Some(true)
        );
        assert_eq!(
            sniff_keyframe(Codec::H265, &[0, 0, 1, 0x02, 0x01]),
            Some(false)
        );
        assert_eq!(
            sniff_keyframe(Codec::Av1, &[0x12, 0, 0x32, 0x01, 0x10]),
            Some(true)
        );
        assert_eq!(sniff_keyframe(Codec::Av1, &[0x32, 0x01, 0x30]), Some(false));
    }

    #[test]
    fn jpeg_round_trip() {
        let w = 64;
        let h = 48;
        let mut img = I420Buffer::new(w, h);
        let yl = img.y_len();
        let ul = img.uv_len();
        img.data[..yl].fill(200);
        img.data[yl..yl + ul].fill(100);
        img.data[yl + ul..].fill(150);
        let src = YuvImage {
            pixels: &img.data[..],
            width: w,
            align: 1,
            height: h,
            subsamp: Subsamp::Sub2x2,
        };
        let jpeg = turbojpeg::compress_yuv(src, 90).unwrap();
        let mut dec = JpegDecoder::new().unwrap();
        assert!(dec.decode(&jpeg).unwrap());
        let v = dec.frame().unwrap();
        assert_eq!((v.width, v.height), (w, h));
        assert!(v.full_range);
        let mid = v.y[(h / 2) * v.y_stride + w / 2];
        assert!((mid as i32 - 200).abs() <= 3, "luma {}", mid);
        let cu = v.u[(h / 4) * v.uv_stride + w / 4];
        assert!((cu as i32 - 100).abs() <= 3, "cb {}", cu);
    }

    /// A JPEG whose header claims a picture past `MAX_JPEG_SIDE` is refused before its
    /// buffers are sized from the claim (65535x65535 would ask 6 GiB of the session host).
    #[test]
    fn a_jpeg_claiming_a_huge_picture_is_refused_before_allocating() {
        let (w, h) = (16usize, 16usize);
        let img = I420Buffer::new(w, h);
        let src = YuvImage {
            pixels: &img.data[..],
            width: w,
            align: 1,
            height: h,
            subsamp: Subsamp::Sub2x2,
        };
        let mut jpeg = turbojpeg::compress_yuv(src, 90).unwrap().to_vec();
        // SOF0: FF C0, length, precision, then height and width.
        let sof = jpeg.windows(2).position(|m| m == [0xFF, 0xC0]).unwrap();
        jpeg[sof + 5..sof + 9].copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
        let mut dec = JpegDecoder::new().unwrap();
        let err = dec.decode(&jpeg).unwrap_err();
        assert!(matches!(err, DecodeError::Corrupt(_)), "{err:?}");
        assert!(dec.out.data.len() < 1 << 20, "{} bytes", dec.out.data.len());
        assert!(dec.rgb.len() < 1 << 20, "{} bytes", dec.rgb.len());
    }

    /// Every inter-coded codec has a decoder, and none holds a picture before decoding one.
    #[test]
    fn a_decoder_constructs_for_every_video_codec() {
        for c in Codec::VIDEO {
            let d = VideoDecoder::new(c).unwrap();
            assert_eq!(d.codec(), c);
            assert!(d.frame().is_none());
            assert_eq!(d.color_tags(), None);
        }
        assert!(VideoDecoder::new(Codec::Jpeg).is_err());
    }

    #[test]
    fn h264_round_trip_through_openh264() {
        use openh264::encoder::Encoder;
        use openh264::formats::YUVBuffer;
        let (w, h) = (64usize, 48usize);
        let mut yuv = vec![0u8; w * h * 3 / 2];
        yuv[..w * h].fill(145);
        yuv[w * h..w * h + w * h / 4].fill(54);
        yuv[w * h + w * h / 4..].fill(34);
        let source = YUVBuffer::from_vec(yuv, w, h);
        let mut enc = Encoder::new().unwrap();
        let mut dec = VideoDecoder::new(Codec::H264).unwrap();
        let mut decoded = 0;
        for _ in 0..6 {
            let bitstream = enc.encode(&source).unwrap().to_vec();
            assert!(!bitstream.is_empty());
            if dec.decode(&bitstream).unwrap() {
                decoded += 1;
                let v = dec.frame().unwrap();
                assert_eq!((v.width, v.height), (w, h));
                assert!(!v.full_range);
                let y = v.y[(h / 2) * v.y_stride + w / 2];
                let u = v.u[(h / 4) * v.uv_stride + w / 4];
                let vv = v.v[(h / 4) * v.uv_stride + w / 4];
                assert!(
                    (y as i32 - 145).abs() <= 6
                        && (u as i32 - 54).abs() <= 6
                        && (vv as i32 - 34).abs() <= 6,
                    "yuv {} {} {}",
                    y,
                    u,
                    vv
                );
            }
        }
        assert!(decoded >= 5, "decoded {} of 6 frames", decoded);
    }

    /// A Constrained Baseline H.264 stream of `frames` access units at `mbs` macroblocks square,
    /// its fields in the order of the specification's syntax tables: an SPS with picture order
    /// count type 0 and two reference frames, a CAVLC PPS, an IDR of I_PCM macroblocks holding
    /// `sample`, then P pictures of skipped macroblocks.
    fn h264_skipping_stream(mbs: u32, frames: u32, sample: u8) -> Vec<Vec<u8>> {
        struct Bits(Vec<u8>, usize);
        impl Bits {
            fn u(&mut self, value: u32, count: usize) {
                for shift in (0..count).rev() {
                    if self.1.is_multiple_of(8) {
                        self.0.push(0);
                    }
                    if (value >> shift) & 1 != 0 {
                        *self.0.last_mut().unwrap() |= 0x80 >> (self.1 % 8);
                    }
                    self.1 += 1;
                }
            }
            fn ue(&mut self, value: u32) {
                let coded = value + 1;
                let length = 32 - coded.leading_zeros() as usize;
                self.u(0, length - 1);
                self.u(coded, length);
            }
            fn align(&mut self) {
                while !self.1.is_multiple_of(8) {
                    self.u(0, 1);
                }
            }
            fn nal(mut self, header: u8) -> Vec<u8> {
                self.u(1, 1);
                self.align();
                let mut out = vec![0, 0, 0, 1, header];
                let mut zeros = 0;
                for byte in self.0 {
                    if zeros >= 2 && byte <= 3 {
                        out.push(3);
                        zeros = 0;
                    }
                    zeros = if byte == 0 { zeros + 1 } else { 0 };
                    out.push(byte);
                }
                out
            }
        }
        let mut sps = Bits(Vec::new(), 0);
        sps.u(66, 8);
        sps.u(0xc0, 8);
        sps.u(40, 8);
        for value in [0, 0, 0, 0, 2] {
            sps.ue(value);
        }
        sps.u(0, 1);
        sps.ue(mbs - 1);
        sps.ue(mbs - 1);
        sps.u(0b1100, 4);
        let mut pps = Bits(Vec::new(), 0);
        pps.ue(0);
        pps.ue(0);
        pps.u(0, 2);
        for value in [0, 0, 0] {
            pps.ue(value);
        }
        pps.u(0, 3);
        for value in [0, 0, 0] {
            pps.ue(value);
        }
        pps.u(0, 3);
        let mut idr = Bits(Vec::new(), 0);
        for value in [0, 7, 0] {
            idr.ue(value);
        }
        idr.u(0, 4);
        idr.ue(0);
        idr.u(0, 4);
        idr.u(0, 2);
        idr.ue(0);
        for _ in 0..mbs * mbs {
            idr.ue(25);
            idr.align();
            for _ in 0..384 {
                idr.u(sample as u32, 8);
            }
        }
        let mut units = vec![[sps.nal(0x67), pps.nal(0x68), idr.nal(0x65)].concat()];
        for n in 1..frames {
            let mut p = Bits(Vec::new(), 0);
            for value in [0, 5, 0] {
                p.ue(value);
            }
            p.u(n % 16, 4);
            p.u(2 * n % 16, 4);
            p.u(0, 3);
            p.ue(0);
            p.ue(mbs * mbs);
            units.push(p.nal(0x41));
        }
        units
    }

    /// A prefix NAL unit naming dependency layer 1, which only a scalable stream carries, leaves
    /// every picture after it decoding, a key frame's included.
    #[test]
    fn h264_scalable_extension_units_never_retarget_the_decoder() {
        let mut units = h264_skipping_stream(2, 12, 0x60);
        units[4].splice(0..0, [0, 0, 0, 1, 0x4e, 0x80, 0x90, 0x07, 0x20]);
        let mut dec = VideoDecoder::new(Codec::H264).unwrap();
        for (n, unit) in units.iter().chain(&units).enumerate() {
            match dec.decode(unit) {
                Ok(true) => {}
                other => panic!("access unit {n}: {other:?}"),
            }
        }
    }

    /// Garbage is a corrupt packet, never a decoder to throw away.
    #[test]
    fn a_decoder_rejects_garbage_without_fatal() {
        for c in Codec::VIDEO {
            let mut d = VideoDecoder::new(c).unwrap();
            match d.decode(&[1, 2, 3, 4, 5, 6, 7, 8]) {
                Ok(false) | Err(DecodeError::Corrupt(_)) => {}
                Ok(true) => panic!("{c:?}: garbage decoded to a frame"),
                Err(DecodeError::Fatal(e)) => panic!("{c:?}: fatal: {}", e),
            }
        }
    }

    /// The VP8 color bits are the first two bool-coded fields of a key frame: a header whose
    /// first partition begins with two zero bits reads as BT.601 at limited range.
    /// A VP9 profile 2 key frame, 10-bit 4:2:0 at 320x240, is refused rather than read as 8-bit:
    /// a libvpx built with high bit depth hands it back in 16-bit samples, and one built without
    /// cannot decode it.
    #[test]
    fn a_10_bit_vp9_frame_never_decodes_as_8_bit() {
        const KEY: [u8; 77] = [
            0x92, 0x49, 0x83, 0x42, 0x00, 0x09, 0xf8, 0x07, 0x7b, 0x03, 0x1c, 0x12, 0x0e, 0x0c,
            0x2c, 0x00, 0x00, 0x48, 0x47, 0xa8, 0x3d, 0x8f, 0xc8, 0xc0, 0x0c, 0x30, 0x00, 0x6b,
            0x41, 0x13, 0x20, 0x95, 0xe2, 0x6a, 0x41, 0xdc, 0xb8, 0x00, 0x11, 0x50, 0x1b, 0x55,
            0x82, 0x76, 0x20, 0x4f, 0xd6, 0xa9, 0x82, 0x10, 0xc5, 0xe9, 0x8f, 0xe0, 0xa3, 0xf7,
            0xcd, 0x66, 0x00, 0xbc, 0xd3, 0xeb, 0x11, 0x3a, 0xb5, 0xed, 0x25, 0x7b, 0x25, 0x6d,
            0xa9, 0x8e, 0xf7, 0xf7, 0xfa, 0x2b, 0x10,
        ];
        let mut d = VideoDecoder::new(Codec::Vp9).unwrap();
        assert!(
            !matches!(d.decode(&KEY), Ok(true)),
            "a 10-bit picture was handed on as an 8-bit one"
        );
    }

    #[test]
    fn vp8_color_bits_read_from_the_first_partition() {
        let mut key = vec![0x10, 0, 0, 0x9d, 0x01, 0x2a, 64, 0, 48, 0];
        key.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        assert_eq!(vp8_color_bits(&key), Some((false, false)));
        key[10] = 0xff;
        key[11] = 0xff;
        assert_eq!(vp8_color_bits(&key), Some((true, true)));
        assert_eq!(
            vp8_color_bits(&[0x11, 0, 0]),
            None,
            "an inter frame carries no header"
        );
    }
}
