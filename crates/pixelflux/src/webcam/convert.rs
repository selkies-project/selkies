//! Normalization of decoded frames into the virtual device's fixed format.
//!
//! The device is a fixed-function camera: one raw pixel format at one size. Whatever the client
//! camera delivers (any size, full- or limited-range luma) is fitted into that: the frame is scaled
//! to fit with black bars when its size differs, full-range (JPEG) samples are compressed into the
//! limited range every V4L2 consumer assumes, and the planes are emitted as I420, NV12, or YUYV.
//! The common path — a frame already at the device size, limited range, I420 device — is a plain
//! plane copy straight into the ring slot.

use yuv::{BufferStoreMut, YuvPackedImageMut, YuvPlanarImage};

use super::ring::{V4L2_PIX_FMT_MJPEG, V4L2_PIX_FMT_NV12, V4L2_PIX_FMT_YUYV};

/// Borrowed I420 planes with arbitrary strides.
#[derive(Clone, Copy)]
pub struct I420View<'a> {
    pub width: usize,
    pub height: usize,
    pub y: &'a [u8],
    pub u: &'a [u8],
    pub v: &'a [u8],
    pub y_stride: usize,
    pub uv_stride: usize,
    /// Samples use the full 0..255 range (JPEG) instead of the limited 16..235 / 16..240 range.
    pub full_range: bool,
}

impl<'a> I420View<'a> {
    pub fn chroma_width(&self) -> usize {
        self.width.div_ceil(2)
    }

    pub fn chroma_height(&self) -> usize {
        self.height.div_ceil(2)
    }
}

/// Target geometry: the device format the ring advertises.
#[derive(Clone, Copy, Debug)]
pub struct DeviceFormat {
    pub width: usize,
    pub height: usize,
    pub fourcc: u32,
}

impl DeviceFormat {
    /// Bytes of one device frame; for MJPEG, of the I420 picture the JPEG is encoded from.
    pub fn frame_bytes(&self) -> usize {
        match self.fourcc {
            V4L2_PIX_FMT_YUYV => self.width * 2 * self.height,
            _ => self.width * self.height + 2 * (self.width.div_ceil(2) * self.height.div_ceil(2)),
        }
    }

    /// JPEG (JFIF) carries full-range samples; every raw device format is limited range.
    pub fn full_range(&self) -> bool {
        self.fourcc == V4L2_PIX_FMT_MJPEG
    }
}

/// Owned, tightly packed I420 buffer used as the normalization scratch.
pub struct I420Buffer {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
}

impl I420Buffer {
    pub fn new(width: usize, height: usize) -> Self {
        let mut b = I420Buffer {
            width,
            height,
            data: Vec::new(),
        };
        b.resize(width, height);
        b
    }

    pub fn resize(&mut self, width: usize, height: usize) {
        self.width = width;
        self.height = height;
        self.data.resize(
            width * height + 2 * (width.div_ceil(2) * height.div_ceil(2)),
            0,
        );
    }

    pub fn y_len(&self) -> usize {
        self.width * self.height
    }

    pub fn uv_len(&self) -> usize {
        self.width.div_ceil(2) * self.height.div_ceil(2)
    }

    pub fn view(&self, full_range: bool) -> I420View<'_> {
        let (y, rest) = self.data.split_at(self.y_len());
        let (u, v) = rest.split_at(self.uv_len());
        I420View {
            width: self.width,
            height: self.height,
            y,
            u,
            v,
            y_stride: self.width,
            uv_stride: self.width.div_ceil(2),
            full_range,
        }
    }

    /// Paint the whole buffer black in the given range (Y 16 limited, 0 full; chroma 128).
    pub fn fill_black(&mut self, full_range: bool) {
        let y_len = self.y_len();
        self.data[..y_len].fill(if full_range { 0 } else { 16 });
        self.data[y_len..].fill(128);
    }
}

/// Full-range to limited-range lookup tables (BT.601/709 quantization; the matrix is unchanged).
struct RangeLut {
    luma: [u8; 256],
    chroma: [u8; 256],
    luma_up: [u8; 256],
    chroma_up: [u8; 256],
}

fn range_lut() -> &'static RangeLut {
    static LUT: std::sync::OnceLock<RangeLut> = std::sync::OnceLock::new();
    LUT.get_or_init(|| {
        let mut luma = [0u8; 256];
        let mut chroma = [0u8; 256];
        let mut luma_up = [0u8; 256];
        let mut chroma_up = [0u8; 256];
        for i in 0..256usize {
            luma[i] = (16 + (i * 219 + 127) / 255) as u8;
            let c = i as i32 - 128;
            let scaled = if c >= 0 {
                (c * 224 + 127) / 255
            } else {
                -((-c * 224 + 127) / 255)
            };
            chroma[i] = (128 + scaled) as u8;
            let ly = i as i32 - 16;
            luma_up[i] = ((ly * 255 + 109) / 219).clamp(0, 255) as u8;
            let up = if c >= 0 {
                (c * 255 + 112) / 224
            } else {
                -((-c * 255 + 112) / 224)
            };
            chroma_up[i] = (128 + up).clamp(0, 255) as u8;
        }
        RangeLut {
            luma,
            chroma,
            luma_up,
            chroma_up,
        }
    })
}

/// The LUTs that move `src` samples into the device's range: full-range JPEG into a
/// limited-range raw device, limited-range video into the full-range MJPEG device, or nothing
/// when the two agree.
fn range_luts(
    src_full: bool,
    dev: &DeviceFormat,
) -> (Option<&'static [u8; 256]>, Option<&'static [u8; 256]>) {
    let lut = range_lut();
    match (src_full, dev.full_range()) {
        (true, false) => (Some(&lut.luma), Some(&lut.chroma)),
        (false, true) => (Some(&lut.luma_up), Some(&lut.chroma_up)),
        _ => (None, None),
    }
}

/// Copy a plane row by row, optionally through a range LUT.
fn copy_plane(
    src: &[u8],
    src_stride: usize,
    width: usize,
    height: usize,
    dst: &mut [u8],
    dst_stride: usize,
    lut: Option<&[u8; 256]>,
) {
    for row in 0..height {
        let s = &src[row * src_stride..row * src_stride + width];
        let d = &mut dst[row * dst_stride..row * dst_stride + width];
        match lut {
            Some(t) => {
                for (o, i) in d.iter_mut().zip(s) {
                    *o = t[*i as usize];
                }
            }
            None => d.copy_from_slice(s),
        }
    }
}

/// Average 2x2 blocks: exact halving, used before bilinear when the ratio exceeds 2 so strong
/// downscales do not alias.
fn halve_plane(
    src: &[u8],
    sw: usize,
    sh: usize,
    sstride: usize,
    dst: &mut Vec<u8>,
) -> (usize, usize) {
    let dw = (sw / 2).max(1);
    let dh = (sh / 2).max(1);
    dst.resize(dw * dh, 0);
    for y in 0..dh {
        let r0 = &src[(2 * y).min(sh - 1) * sstride..];
        let r1 = &src[(2 * y + 1).min(sh - 1) * sstride..];
        let out = &mut dst[y * dw..(y + 1) * dw];
        for (x, o) in out.iter_mut().enumerate() {
            let x0 = (2 * x).min(sw - 1);
            let x1 = (2 * x + 1).min(sw - 1);
            *o = ((r0[x0] as u32 + r0[x1] as u32 + r1[x0] as u32 + r1[x1] as u32 + 2) / 4) as u8;
        }
    }
    (dw, dh)
}

/// Separable bilinear resample of one plane (fixed point 16.16).
#[allow(clippy::too_many_arguments)]
fn bilinear_plane(
    src: &[u8],
    sw: usize,
    sh: usize,
    sstride: usize,
    dst: &mut [u8],
    dw: usize,
    dh: usize,
    dstride: usize,
) {
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
        return;
    }
    if sw == dw && sh == dh {
        copy_plane(src, sstride, sw, sh, dst, dstride, None);
        return;
    }
    let xs: Vec<(usize, usize, u32)> = (0..dw)
        .map(|x| {
            let fx =
                ((x as u64 * 2 + 1) * sw as u64 * 65536 / (dw as u64 * 2)).saturating_sub(32768);
            let x0 = (fx >> 16) as usize;
            let frac = (fx & 0xFFFF) as u32;
            let x0 = x0.min(sw - 1);
            let x1 = (x0 + 1).min(sw - 1);
            (x0, x1, frac)
        })
        .collect();
    for y in 0..dh {
        let fy = ((y as u64 * 2 + 1) * sh as u64 * 65536 / (dh as u64 * 2)).saturating_sub(32768);
        let y0 = ((fy >> 16) as usize).min(sh - 1);
        let y1 = (y0 + 1).min(sh - 1);
        let wy = (fy & 0xFFFF) as u32;
        let r0 = &src[y0 * sstride..y0 * sstride + sw];
        let r1 = &src[y1 * sstride..y1 * sstride + sw];
        let out = &mut dst[y * dstride..y * dstride + dw];
        for (o, &(x0, x1, wx)) in out.iter_mut().zip(&xs) {
            let top = r0[x0] as u32 * (65536 - wx) + r0[x1] as u32 * wx;
            let bot = r1[x0] as u32 * (65536 - wx) + r1[x1] as u32 * wx;
            let v = ((top >> 8) * (65536 - wy) + (bot >> 8) * wy + (1 << 23)) >> 24;
            *o = v.min(255) as u8;
        }
    }
}

/// Resample a plane into `dst` (tightly packed `dw` x `dh`), halving first while the source is more
/// than twice the destination in either dimension.
#[allow(clippy::too_many_arguments)]
fn scale_plane(
    src: &[u8],
    sw: usize,
    sh: usize,
    sstride: usize,
    dst: &mut [u8],
    dw: usize,
    dh: usize,
    tmp: &mut [Vec<u8>; 2],
) {
    let mut cur: Option<(usize, usize, usize)> = None;
    let mut which = 0;
    let (mut w, mut h, mut stride) = (sw, sh, sstride);
    while w >= 2 * dw && h >= 2 * dh && w > 1 && h > 1 {
        let (nw, nh) = {
            let (a, b) = tmp.split_at_mut(1);
            let (out, input) = if which == 0 {
                (&mut b[0], &a[0])
            } else {
                (&mut a[0], &b[0])
            };
            match cur {
                None => halve_plane(src, w, h, stride, out),
                Some(_) => halve_plane(input, w, h, stride, out),
            }
        };
        which ^= 1;
        w = nw;
        h = nh;
        stride = nw;
        cur = Some((w, h, stride));
    }
    match cur {
        None => bilinear_plane(src, sw, sh, sstride, dst, dw, dh, dw),
        Some(_) => {
            let input = if which == 0 { &tmp[0] } else { &tmp[1] };
            bilinear_plane(input, w, h, stride, dst, dw, dh, dw);
        }
    }
}

/// Scratch state kept across frames so normalization allocates only on geometry changes.
pub struct Normalizer {
    fitted: I420Buffer,
    fitted_geometry: Option<(usize, usize, usize, usize)>,
    scaled: [Vec<u8>; 3],
    halves: [Vec<u8>; 2],
}

impl Normalizer {
    pub fn new() -> Self {
        Normalizer {
            fitted: I420Buffer::new(2, 2),
            fitted_geometry: None,
            scaled: [Vec::new(), Vec::new(), Vec::new()],
            halves: [Vec::new(), Vec::new()],
        }
    }

    /// Write `src` into `out` as a `dev` frame; returns the bytes written (0 if `out` is too small).
    pub fn write_frame(&mut self, src: &I420View<'_>, dev: &DeviceFormat, out: &mut [u8]) -> usize {
        let need = dev.frame_bytes();
        if out.len() < need || src.width == 0 || src.height == 0 {
            return 0;
        }
        if src.width == dev.width && src.height == dev.height {
            emit(src, dev, out);
            return need;
        }
        let (dw, dh, ox, oy) = fit(src.width, src.height, dev.width, dev.height);
        if self.fitted.width != dev.width || self.fitted.height != dev.height {
            self.fitted.resize(dev.width, dev.height);
            self.fitted_geometry = None;
        }
        if self.fitted_geometry != Some((dw, dh, ox, oy)) {
            self.fitted.fill_black(dev.full_range());
            self.fitted_geometry = Some((dw, dh, ox, oy));
        }
        let cw = dw.div_ceil(2);
        let ch = dh.div_ceil(2);
        self.scaled[0].resize(dw * dh, 0);
        self.scaled[1].resize(cw * ch, 0);
        self.scaled[2].resize(cw * ch, 0);
        scale_plane(
            src.y,
            src.width,
            src.height,
            src.y_stride,
            &mut self.scaled[0],
            dw,
            dh,
            &mut self.halves,
        );
        scale_plane(
            src.u,
            src.chroma_width(),
            src.chroma_height(),
            src.uv_stride,
            &mut self.scaled[1],
            cw,
            ch,
            &mut self.halves,
        );
        scale_plane(
            src.v,
            src.chroma_width(),
            src.chroma_height(),
            src.uv_stride,
            &mut self.scaled[2],
            cw,
            ch,
            &mut self.halves,
        );
        let (y_lut, c_lut) = range_luts(src.full_range, dev);
        let fw = self.fitted.width;
        let fcw = fw.div_ceil(2);
        let y_len = self.fitted.y_len();
        let uv_len = self.fitted.uv_len();
        let (yp, rest) = self.fitted.data.split_at_mut(y_len);
        let (up, vp) = rest.split_at_mut(uv_len);
        copy_plane(
            &self.scaled[0],
            dw,
            dw,
            dh,
            &mut yp[oy * fw + ox..],
            fw,
            y_lut,
        );
        copy_plane(
            &self.scaled[1],
            cw,
            cw,
            ch,
            &mut up[(oy / 2) * fcw + ox / 2..],
            fcw,
            c_lut,
        );
        copy_plane(
            &self.scaled[2],
            cw,
            cw,
            ch,
            &mut vp[(oy / 2) * fcw + ox / 2..],
            fcw,
            c_lut,
        );
        let fitted = self.fitted.view(dev.full_range());
        emit(&fitted, dev, out);
        need
    }
}

impl Default for Normalizer {
    fn default() -> Self {
        Self::new()
    }
}

/// One upright transform: clockwise quarter turns, then a horizontal mirror.
///
/// Clients encode the camera's pixels as captured and relay the upright
/// transform with each frame; it is applied here, right after decode, where
/// the fit step already absorbs the size swap of odd turns.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Orientation {
    /// Clockwise quarter turns (0..=3) that make the frame upright.
    pub quarter_turns: u8,
    /// Horizontal mirror, applied after the rotation.
    pub hflip: bool,
}

impl Orientation {
    pub const UPRIGHT: Orientation = Orientation {
        quarter_turns: 0,
        hflip: false,
    };

    pub fn is_upright(&self) -> bool {
        self.quarter_turns.is_multiple_of(4) && !self.hflip
    }
}

/// Destination square a quarter turn is taken in, so the source rows it gathers
/// from stay in the innermost cache across the whole square.
const ORIENT_TILE: usize = 32;

/// Rotate one plane clockwise into a tightly packed destination (whose width is
/// `h` for odd turn counts), mirroring each destination row for `hflip`.
///
/// Half turns keep rows as rows and are copied as such. A quarter turn gathers a
/// source column per destination row, so it is taken in tiles: a whole-row pass
/// touches every source row once per output row and spills the cache on any
/// frame that does not fit in it.
fn orient_plane(src: &[u8], stride: usize, w: usize, h: usize, o: Orientation, dst: &mut [u8]) {
    let q = o.quarter_turns % 4;
    let (dw, dh) = if q % 2 == 1 { (h, w) } else { (w, h) };
    if q.is_multiple_of(2) {
        let reversed = (q == 2) != o.hflip;
        for dy in 0..dh {
            let sy = if q == 2 { h - 1 - dy } else { dy };
            let row = &src[sy * stride..sy * stride + w];
            let out = &mut dst[dy * dw..dy * dw + dw];
            if reversed {
                for (dx, o) in out.iter_mut().enumerate() {
                    *o = row[w - 1 - dx];
                }
            } else {
                out.copy_from_slice(row);
            }
        }
        return;
    }
    for ty in (0..dh).step_by(ORIENT_TILE) {
        for tx in (0..dw).step_by(ORIENT_TILE) {
            for dy in ty..(ty + ORIENT_TILE).min(dh) {
                let row = &mut dst[dy * dw..(dy + 1) * dw];
                for dx in tx..(tx + ORIENT_TILE).min(dw) {
                    let (sx, sy) = if q == 1 {
                        (dy, h - 1 - dx)
                    } else {
                        (w - 1 - dy, dx)
                    };
                    row[if o.hflip { dw - 1 - dx } else { dx }] = src[sy * stride + sx];
                }
            }
        }
    }
}

/// Apply `o` to `src`, producing a tightly packed upright copy in `out`. The
/// chroma planes of the rotated picture are exactly the rotated chroma planes
/// (odd-turn chroma dimensions transpose with them), so each plane is oriented
/// independently.
pub fn orient_i420(src: &I420View<'_>, o: Orientation, out: &mut I420Buffer) {
    let q = o.quarter_turns % 4;
    let (dw, dh) = if q % 2 == 1 {
        (src.height, src.width)
    } else {
        (src.width, src.height)
    };
    out.resize(dw, dh);
    let y_len = out.y_len();
    let uv_len = out.uv_len();
    let (yp, rest) = out.data.split_at_mut(y_len);
    let (up, vp) = rest.split_at_mut(uv_len);
    orient_plane(src.y, src.y_stride, src.width, src.height, o, yp);
    orient_plane(
        src.u,
        src.uv_stride,
        src.chroma_width(),
        src.chroma_height(),
        o,
        up,
    );
    orient_plane(
        src.v,
        src.uv_stride,
        src.chroma_width(),
        src.chroma_height(),
        o,
        vp,
    );
}

/// Largest even-aligned size of `sw` x `sh` that fits into `dw` x `dh` preserving aspect, and the
/// even-aligned offsets that center it.
pub fn fit(sw: usize, sh: usize, dw: usize, dh: usize) -> (usize, usize, usize, usize) {
    let (mut w, mut h) = if sw * dh > dw * sh {
        (dw, (sh * dw / sw).max(2))
    } else {
        ((sw * dh / sh).max(2), dh)
    };
    w = (w.min(dw) / 2) * 2;
    h = (h.min(dh) / 2) * 2;
    let ox = ((dw - w) / 2 / 2) * 2;
    let oy = ((dh - h) / 2 / 2) * 2;
    (w, h, ox, oy)
}

/// Emit `src` (already at the device size) as the device fourcc.
fn emit(src: &I420View<'_>, dev: &DeviceFormat, out: &mut [u8]) {
    let (y_lut, c_lut) = range_luts(src.full_range, dev);
    let w = dev.width;
    let h = dev.height;
    let cw = w.div_ceil(2);
    let ch = h.div_ceil(2);
    match dev.fourcc {
        V4L2_PIX_FMT_NV12 => {
            let (yp, uvp) = out.split_at_mut(w * h);
            copy_plane(src.y, src.y_stride, w, h, yp, w, y_lut);
            for row in 0..ch {
                let u = &src.u[row * src.uv_stride..row * src.uv_stride + cw];
                let v = &src.v[row * src.uv_stride..row * src.uv_stride + cw];
                let o = &mut uvp[row * cw * 2..(row + 1) * cw * 2];
                for x in 0..cw {
                    let (uu, vv) = match c_lut {
                        Some(t) => (t[u[x] as usize], t[v[x] as usize]),
                        None => (u[x], v[x]),
                    };
                    o[2 * x] = uu;
                    o[2 * x + 1] = vv;
                }
            }
        }
        V4L2_PIX_FMT_YUYV => {
            let planar = if src.full_range {
                None
            } else {
                Some(YuvPlanarImage {
                    y_plane: src.y,
                    y_stride: src.y_stride as u32,
                    u_plane: src.u,
                    u_stride: src.uv_stride as u32,
                    v_plane: src.v,
                    v_stride: src.uv_stride as u32,
                    width: w as u32,
                    height: h as u32,
                })
            };
            match planar {
                Some(p) => {
                    let mut packed = YuvPackedImageMut {
                        yuy: BufferStoreMut::Borrowed(&mut out[..w * 2 * h]),
                        yuy_stride: (w * 2) as u32,
                        width: w as u32,
                        height: h as u32,
                    };
                    if yuv::yuv420_to_yuyv422(&mut packed, &p).is_ok() {
                        return;
                    }
                    yuyv_scalar(src, w, h, out, y_lut, c_lut);
                }
                None => yuyv_scalar(src, w, h, out, y_lut, c_lut),
            }
        }
        _ => {
            let (yp, rest) = out.split_at_mut(w * h);
            let (up, vp) = rest.split_at_mut(cw * ch);
            copy_plane(src.y, src.y_stride, w, h, yp, w, y_lut);
            copy_plane(src.u, src.uv_stride, cw, ch, up, cw, c_lut);
            copy_plane(src.v, src.uv_stride, cw, ch, vp, cw, c_lut);
        }
    }
}

fn yuyv_scalar(
    src: &I420View<'_>,
    w: usize,
    h: usize,
    out: &mut [u8],
    y_lut: Option<&[u8; 256]>,
    c_lut: Option<&[u8; 256]>,
) {
    let cw = w.div_ceil(2);
    for row in 0..h {
        let y = &src.y[row * src.y_stride..row * src.y_stride + w];
        let u = &src.u[(row / 2) * src.uv_stride..(row / 2) * src.uv_stride + cw];
        let v = &src.v[(row / 2) * src.uv_stride..(row / 2) * src.uv_stride + cw];
        let o = &mut out[row * w * 2..(row + 1) * w * 2];
        for x in 0..w {
            let yy = match y_lut {
                Some(t) => t[y[x] as usize],
                None => y[x],
            };
            let c = if x % 2 == 0 { u[x / 2] } else { v[x / 2] };
            let cc = match c_lut {
                Some(t) => t[c as usize],
                None => c,
            };
            o[2 * x] = yy;
            o[2 * x + 1] = cc;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::ring::V4L2_PIX_FMT_YUV420;
    use super::*;

    fn solid(width: usize, height: usize, y: u8, u: u8, v: u8) -> I420Buffer {
        let mut b = I420Buffer::new(width, height);
        let yl = b.y_len();
        let ul = b.uv_len();
        b.data[..yl].fill(y);
        b.data[yl..yl + ul].fill(u);
        b.data[yl + ul..].fill(v);
        b
    }

    #[test]
    fn mjpeg_device_takes_full_range_samples() {
        let mjpeg = DeviceFormat {
            width: 4,
            height: 2,
            fourcc: V4L2_PIX_FMT_MJPEG,
        };
        let raw = DeviceFormat {
            width: 4,
            height: 2,
            fourcc: V4L2_PIX_FMT_YUV420,
        };
        let mut limited = I420Buffer::new(4, 2);
        limited.data[..8].fill(16);
        limited.data[8..].fill(128);
        let mut out = vec![0u8; mjpeg.frame_bytes()];
        let mut n = Normalizer::new();
        assert_eq!(
            n.write_frame(&limited.view(false), &mjpeg, &mut out),
            mjpeg.frame_bytes()
        );
        assert_eq!((out[0], out[8]), (0, 128));
        limited.data[..8].fill(235);
        n.write_frame(&limited.view(false), &mjpeg, &mut out);
        assert_eq!(out[0], 255);
        let mut full = I420Buffer::new(4, 2);
        full.data[..8].fill(200);
        full.data[8..].fill(60);
        n.write_frame(&full.view(true), &mjpeg, &mut out);
        assert_eq!((out[0], out[8]), (200, 60));
        n.write_frame(&full.view(true), &raw, &mut out);
        assert_eq!(
            (out[0], out[8]),
            (range_lut().luma[200], range_lut().chroma[60])
        );
    }

    #[test]
    fn fit_preserves_aspect_and_alignment() {
        assert_eq!(fit(1280, 720, 1280, 720), (1280, 720, 0, 0));
        assert_eq!(fit(640, 480, 1280, 720), (960, 720, 160, 0));
        assert_eq!(fit(720, 1280, 1280, 720), (404, 720, 438, 0));
        assert_eq!(fit(1920, 1080, 640, 480), (640, 360, 0, 60));
        let (w, h, ox, oy) = fit(333, 777, 1280, 720);
        assert!(w % 2 == 0 && h % 2 == 0 && ox % 2 == 0 && oy % 2 == 0);
        assert!(w + ox <= 1280 && h + oy <= 720);
    }

    #[test]
    fn same_size_i420_is_a_plain_copy() {
        let src = solid(8, 4, 100, 60, 200);
        let dev = DeviceFormat {
            width: 8,
            height: 4,
            fourcc: V4L2_PIX_FMT_YUV420,
        };
        let mut out = vec![0u8; dev.frame_bytes() + 7];
        let n = Normalizer::new().write_frame(&src.view(false), &dev, &mut out);
        assert_eq!(n, 8 * 4 * 3 / 2);
        assert_eq!(&out[..n], &src.data[..]);
    }

    #[test]
    fn full_range_is_compressed_to_limited() {
        let src = solid(4, 2, 255, 0, 255);
        let dev = DeviceFormat {
            width: 4,
            height: 2,
            fourcc: V4L2_PIX_FMT_YUV420,
        };
        let mut out = vec![0u8; dev.frame_bytes()];
        Normalizer::new().write_frame(&src.view(true), &dev, &mut out);
        assert_eq!(out[0], 235);
        assert_eq!(out[8], 16);
        assert_eq!(out[10], 240);
        let black = solid(4, 2, 0, 128, 128);
        Normalizer::new().write_frame(&black.view(true), &dev, &mut out);
        assert_eq!((out[0], out[8], out[10]), (16, 128, 128));
    }

    #[test]
    fn nv12_and_yuyv_interleave() {
        let src = solid(4, 2, 50, 60, 70);
        let nv12 = DeviceFormat {
            width: 4,
            height: 2,
            fourcc: V4L2_PIX_FMT_NV12,
        };
        let mut out = vec![0u8; nv12.frame_bytes()];
        assert_eq!(
            Normalizer::new().write_frame(&src.view(false), &nv12, &mut out),
            12
        );
        assert_eq!(&out[..8], &[50; 8]);
        assert_eq!(&out[8..], &[60, 70, 60, 70]);
        let yuyv = DeviceFormat {
            width: 4,
            height: 2,
            fourcc: V4L2_PIX_FMT_YUYV,
        };
        let mut out = vec![0u8; yuyv.frame_bytes()];
        assert_eq!(
            Normalizer::new().write_frame(&src.view(false), &yuyv, &mut out),
            16
        );
        assert_eq!(&out[..8], &[50, 60, 50, 70, 50, 60, 50, 70]);
        let mut out2 = vec![0u8; yuyv.frame_bytes()];
        Normalizer::new().write_frame(&src.view(true), &yuyv, &mut out2);
        assert_eq!(out2[0], range_lut().luma[50]);
        assert_eq!(out2[1], range_lut().chroma[60]);
    }

    #[test]
    fn letterbox_scales_and_pads_black() {
        let src = solid(8, 8, 200, 90, 160);
        let dev = DeviceFormat {
            width: 16,
            height: 8,
            fourcc: V4L2_PIX_FMT_YUV420,
        };
        let mut out = vec![0u8; dev.frame_bytes()];
        let mut n = Normalizer::new();
        assert_eq!(
            n.write_frame(&src.view(false), &dev, &mut out),
            dev.frame_bytes()
        );
        assert_eq!(out[0], 16);
        assert_eq!(out[4 + 3 * 16], 200);
        assert_eq!(out[15], 16);
        let uv = 16 * 8;
        assert_eq!(out[uv], 128);
        assert_eq!(out[uv + 2 + 8 * 2], 90);
        assert_eq!(out[uv + 8 * 4 + 2 + 8 * 2], 160);
        assert_eq!(
            n.write_frame(&src.view(false), &dev, &mut out),
            dev.frame_bytes()
        );
        assert_eq!(out[4 + 3 * 16], 200);
    }

    #[test]
    fn orient_rotates_clockwise_then_mirrors() {
        let mut src = I420Buffer::new(2, 2);
        src.data[..4].copy_from_slice(&[1, 2, 3, 4]);
        src.data[4] = 10;
        src.data[5] = 20;
        let mut out = I420Buffer::new(2, 2);
        let v = src.view(false);
        orient_i420(
            &v,
            Orientation {
                quarter_turns: 1,
                hflip: false,
            },
            &mut out,
        );
        assert_eq!(&out.data[..4], &[3, 1, 4, 2]);
        orient_i420(
            &v,
            Orientation {
                quarter_turns: 2,
                hflip: false,
            },
            &mut out,
        );
        assert_eq!(&out.data[..4], &[4, 3, 2, 1]);
        orient_i420(
            &v,
            Orientation {
                quarter_turns: 3,
                hflip: false,
            },
            &mut out,
        );
        assert_eq!(&out.data[..4], &[2, 4, 1, 3]);
        orient_i420(
            &v,
            Orientation {
                quarter_turns: 0,
                hflip: true,
            },
            &mut out,
        );
        assert_eq!(&out.data[..4], &[2, 1, 4, 3]);
        assert_eq!((out.data[4], out.data[5]), (10, 20));
        assert!(Orientation::UPRIGHT.is_upright());
        assert!(
            !Orientation {
                quarter_turns: 2,
                hflip: false
            }
            .is_upright()
        );
    }

    #[test]
    fn orient_quarter_turn_spans_tiles() {
        // Wider and taller than one tile, so the gather is checked across tile
        // boundaries rather than inside a single square.
        let (w, h) = (ORIENT_TILE * 2 + 6, ORIENT_TILE + 8);
        let mut src = I420Buffer::new(w, h);
        for (i, b) in src.data[..w * h].iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let v = src.view(false);
        let mut out = I420Buffer::new(2, 2);
        for (q, hflip) in [(1u8, false), (1, true), (3, false), (3, true)] {
            orient_i420(
                &v,
                Orientation {
                    quarter_turns: q,
                    hflip,
                },
                &mut out,
            );
            assert_eq!((out.width, out.height), (h, w));
            for dy in 0..w {
                for dx in 0..h {
                    let (sx, sy) = if q == 1 {
                        (dy, h - 1 - dx)
                    } else {
                        (w - 1 - dy, dx)
                    };
                    let dst_x = if hflip { h - 1 - dx } else { dx };
                    assert_eq!(
                        out.data[dy * h + dst_x],
                        v.y[sy * v.y_stride + sx],
                        "q={} hflip={} dx={} dy={}",
                        q,
                        hflip,
                        dx,
                        dy
                    );
                }
            }
        }
    }

    #[test]
    fn orient_swaps_dimensions_for_odd_turns() {
        let mut src = I420Buffer::new(4, 2);
        for (i, b) in src.data[..8].iter_mut().enumerate() {
            *b = i as u8;
        }
        let mut out = I420Buffer::new(2, 2);
        orient_i420(
            &src.view(true),
            Orientation {
                quarter_turns: 1,
                hflip: false,
            },
            &mut out,
        );
        assert_eq!((out.width, out.height), (2, 4));
        assert_eq!(&out.data[..8], &[4, 0, 5, 1, 6, 2, 7, 3]);
        assert!(out.view(true).full_range);
    }

    #[test]
    fn downscale_averages() {
        let mut src = I420Buffer::new(64, 64);
        for (i, b) in src.data.iter_mut().enumerate() {
            *b = if i < 64 * 64 {
                ((i % 64) * 4) as u8
            } else {
                128
            };
        }
        let dev = DeviceFormat {
            width: 16,
            height: 16,
            fourcc: V4L2_PIX_FMT_YUV420,
        };
        let mut out = vec![0u8; dev.frame_bytes()];
        assert_eq!(
            Normalizer::new().write_frame(&src.view(false), &dev, &mut out),
            dev.frame_bytes()
        );
        assert!(out[0] < out[8] && out[8] < out[15]);
        assert!(out[..256].iter().all(|v| *v <= 252));
        assert!(out[256..].iter().all(|v| *v == 128));
    }
}
