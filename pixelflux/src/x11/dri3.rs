/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! DRI3 zero-copy X11 capture: the X server blits the screen into GPU buffers this process
//! allocated, and the hardware encoder reads them where they lie.
//!
//! It serves a server whose screen lives on the GPU (XLibre's Xvfb with glamor, an Xorg on a
//! DRM driver), where the XShm path in [`super`] would read the screen back into host memory
//! for the hardware session to upload again. A small pool of dmabufs is allocated through GBM
//! on the render node the server draws with, each is wrapped in an X pixmap
//! (`PixmapFromBuffers`), and every frame is one `CopyArea` from the root window into the next
//! pixmap, which glamor runs as a GPU-to-GPU blit. The encoder then imports the dmabuf in place,
//! through the same [`FrameEncoder::encode_dmabuf`] the Wayland zero-copy path uses, so a frame
//! is never touched by the CPU between the screen and the bitstream.
//!
//! Completion is waited for with a one-pixel `GetImage` of the destination pixmap: the server
//! answers only after the blit it ordered before has landed, so the encoder never reads a
//! half-copied frame, and no fence object or implicit-sync assumption is involved.
//!
//! Where the server offers SELKIES-SEMAPHORE (the images' Xvfb) and the encoder can wait on a GPU
//! semaphore (NVENC), the server signals one the encoder made after each blit instead, and the
//! encode is queued at once behind a wait on it. Under a GPU-bound client each GPU job that has
//! to wait for the CPU to submit it waits a draw of that client for its turn, so the GetImage
//! cost two draws a frame where the semaphore costs one: an uncapped 72 fps client streamed at
//! 36 fps with the GetImage. A checked Signal request is the round trip the wait follows, which
//! answers once the server has queued the signal, not once the GPU ran it: a wait queued before
//! the server's signal lost its turn as well.
//!
//! Nothing here reads pixels, so what stands in for the XShm path's per-stripe content hashing is
//! the Damage extension on the root window: the server's report is both what says a frame is worth
//! encoding and what ends the wait for one, within the pacing [`crate::pace`] keeps, so a change is
//! published as it lands rather than on the next tick. Its region says how much changed, so a
//! blinking caret stays a small change and a still screen is cleaned up around it. The XFixes
//! cursor is composited by the server through Render, on the GPU, so the `capture_cursor` overlay
//! costs no readback either; the server keeps its own software cursor out of a copy from the
//! screen, so the cursor is never drawn twice.
//!
//! Everything that can be checked is checked before a frame is delivered, and the path is
//! declined otherwise: a codec no hardware engine serves, software encoding, a server without
//! DRI3, Damage, or Render, a server drawing on a GPU other than the encode node's, a buffer
//! the server will not import, or a first frame the encoder cannot read. A watermark is not among
//! them: it is composited by the server through Render, as the cursor is. The capture then runs the XShm path with nothing half-built.
//!
//! A DRI3 1.0 server (the NVIDIA X driver before 595) takes no layout with a buffer, so the pool is
//! linear there and each buffer goes over with its stride alone.

use std::ffi::c_void;
use std::fs::File;
use std::io::IoSlice;
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use gbm::{BufferObject, Device as RawGbmDevice, Format as GbmFormat};
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::gbm::GbmDevice;
use smithay::backend::egl::EGLDisplay;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::dri3::ConnectionExt as Dri3Ext;
use x11rb::protocol::render::{self, ConnectionExt as RenderExt};
use x11rb::protocol::xfixes::ConnectionExt as XfixesExt;
use x11rb::protocol::xproto::{self, ConnectionExt as XprotoExt, ImageFormat, ImageOrder};
use x11rb::rust_connection::RustConnection;

use super::{
    Controls, RootDamage, captured_damage, clamp_offset, cursor_image_origin, require_32bpp,
    resolve_dims, take_tick, wait_for_frame,
};
use crate::CaptureSettings;
use crate::encoders::software::{EncodedStripe, FrameTiming, StripeState};
use crate::encoders::{self, FrameEncoder, FrameSource};
use crate::pace::FramePace;
use crate::pipeline::{Damage, EncoderQuality, decide_hw_fullframe};
use crate::recording_sink::RecordingSink;

/// GPU buffers the screen is blitted into, used round-robin so an encoder still reading the
/// previous frame never has the next blitted over it.
const POOL_N: usize = 3;

/// Frames between framebuffer-size polls, matching the XShm path's geometry cadence.
const GEOMETRY_POLL_FRAMES: i32 = 30;

/// Consecutive X request failures before the capture reports itself dead: the server is gone
/// rather than momentarily late, and the Python watchdog restarts the display.
const MAX_X_FAILURES: u32 = 5;

/// One dmabuf the server blits into: the GBM allocation, the encoder-side description of it,
/// and the pixmap and Render picture the server knows it as.
struct GpuBuffer {
    _bo: BufferObject<()>,
    dmabuf: Dmabuf,
    pixmap: xproto::Pixmap,
    picture: render::Picture,
}

/// An ARGB image the server composites over each frame through Render: the XFixes cursor,
/// re-uploaded whenever it changes, or the watermark, uploaded once.
struct Sprite {
    pixmap: xproto::Pixmap,
    picture: render::Picture,
    gc: xproto::Gcontext,
    width: u16,
    height: u16,
}

/// The XFixes cursor image uploaded once per cursor change as an ARGB pixmap the server
/// composites over each frame.
struct CursorSprite {
    pixmap: xproto::Pixmap,
    picture: render::Picture,
    gc: xproto::Gcontext,
    width: u16,
    height: u16,
    serial: u32,
}

/// The X side of the capture: the private connection and the server objects the blit needs.
struct XScreen {
    conn: RustConnection,
    root: xproto::Window,
    depth: u8,
    byte_order: ImageOrder,
    gc: xproto::Gcontext,
    /// The server's report that the root changed, which both ends a frame's wait early and
    /// says whether to encode one at all: nothing here reads pixels, so there is no content
    /// hash to stand in for it.
    damage: RootDamage,
    /// The Render format of the screen's depth, for the destination pictures.
    rgb_format: render::Pictformat,
    /// The Render format of a premultiplied ARGB cursor image.
    argb_format: render::Pictformat,
    /// What the server accepts for buffers of the screen's depth; empty leaves the driver's
    /// choice and the server learns the layout from the modifier the allocation reports.
    modifiers: Vec<u64>,
    /// Whether the server takes a buffer with its modifier (DRI3 1.2). A DRI3 1.0 server
    /// (the NVIDIA X driver before 595) learns no layout with a buffer, so the pool is
    /// linear there and each buffer goes over with its stride alone (`PixmapFromBuffer`).
    explicit: bool,
    device: String,
    /// SELKIES-SEMAPHORE's major opcode, where the server offers it.
    semaphore_opcode: Option<u8>,
}

/// The images' Xvfb extension for GPU semaphores shared with the screen's renderer.
const SEMAPHORE_EXTENSION: &str = "SELKIES-SEMAPHORE";

/// A running zero-copy capture, on the thread that owns every context in it.
///
/// Field order is drop order: the encoder releases its imports before the buffers they
/// describe go, and both go before the GBM device and EGL display they were made on.
struct GpuCapture {
    /// `None` only while a rebuild is replacing the session; a rebuild that fails ends the
    /// capture before another frame is encoded.
    encoder: Option<FrameEncoder>,
    buffers: Vec<GpuBuffer>,
    cursor: Option<CursorSprite>,
    /// The watermark image and where it sits, and the pixmap the server composites it from.
    /// Held rather than blended into host pixels, which is what keeps this path zero-copy.
    overlay: crate::encoders::overlay::OverlayState,
    watermark: Option<Sprite>,
    x: XScreen,
    gbm: RawGbmDevice<File>,
    _egl: Option<EGLDisplay>,
    egl_display: *const c_void,
    next: usize,
    /// The live session: its geometry is what is captured and encoded, and its rate and quality
    /// fields follow the cross-thread controls.
    settings: CaptureSettings,
    /// The geometry that was asked for, which a region change updates and every region is
    /// resolved from, kept apart from `settings` so a capture following the root keeps following.
    request: CaptureSettings,
    cap_x: i16,
    cap_y: i16,
    root_w: u16,
    root_h: u16,
    /// The server's id for the encoder's blit semaphore, which it signals after each blit for
    /// the encode to wait on; `None` waits for each blit with a GetImage.
    blit_semaphore: Option<u32>,
}

/// Take `chosen` out of the modifiers the next allocation may pick. A choice the list never
/// named came from the driver itself, so the whole list goes and the driver picks again
/// unconstrained.
fn prune_modifier(modifiers: &mut Vec<u64>, chosen: u64) {
    let before = modifiers.len();
    modifiers.retain(|&m| m != chosen);
    if modifiers.len() == before {
        modifiers.clear();
    }
}

/// Whether `modifier` is one of Intel's compressed (CCS) layouts. The plane count does not tell:
/// DG2 and later keep the compression state outside the buffer, so theirs are single-plane, and
/// the media driver reads such a buffer as if it were uncompressed. Intel's uncompressed
/// layouts are the few named here; every other one it defines is a CCS variant.
fn intel_compressed(modifier: u64) -> bool {
    const INTEL: u64 = 0x01;
    const X_TILED: u64 = 1;
    const Y_TILED: u64 = 2;
    const YF_TILED: u64 = 3;
    const TILE4: u64 = 9;
    let layout = modifier & 0x00ff_ffff_ffff_ffff;
    modifier >> 56 == INTEL && !matches!(layout, X_TILED | Y_TILED | YF_TILED | TILE4)
}

/// Why the DRI3 path was not taken, for the one line that says so.
fn declined(reason: &str) -> Option<GpuCapture> {
    println!("[X11] Zero-copy capture (DRI3) unavailable: {reason}; capturing through XShm.");
    crate::report::capture_declined("DRI3", reason);
    None
}

fn x_err(what: &str, e: impl std::fmt::Display) -> String {
    format!("{what}: {e}")
}

/// The DRM device a file descriptor names, as the kernel numbers it.
fn device_of_fd(fd: &OwnedFd) -> Result<u64, String> {
    let file = File::from(
        fd.try_clone()
            .map_err(|e| x_err("dup of the DRI3 device", e))?,
    );
    file.metadata()
        .map(|m| m.rdev())
        .map_err(|e| x_err("fstat of the DRI3 device", e))
}

/// Whether two DRM nodes belong to one GPU: an Xorg whose glamor runs on the modesetting
/// driver hands DRI3 clients its primary node, where the encoder opens the render node.
fn same_gpu(a: u64, b: u64) -> bool {
    let gpu = |dev: u64| {
        std::fs::canonicalize(format!(
            "/sys/dev/char/{}:{}/device",
            libc::major(dev),
            libc::minor(dev)
        ))
        .ok()
    };
    a == b || gpu(a).is_some_and(|g| gpu(b) == Some(g))
}

impl XScreen {
    /// Connect and negotiate everything the blit needs, or say what the server lacks.
    fn open(node: i32) -> Result<(Self, OwnedFd), String> {
        let (conn, screen_num) =
            x11rb::connect(None).map_err(|e| x_err("X11 connect failed", e))?;
        require_32bpp(&conn, screen_num)?;
        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;
        let depth = screen.root_depth;
        let byte_order = conn.setup().image_byte_order;

        let dri3 = conn
            .dri3_query_version(1, 2)
            .map_err(|e| x_err("DRI3 query", e))?
            .reply()
            .map_err(|e| x_err("the server has no DRI3", e))?;
        if dri3.major_version < 1 {
            return Err(format!(
                "the server's DRI3 is {}.{}",
                dri3.major_version, dri3.minor_version
            ));
        }
        let explicit = dri3.major_version > 1 || dri3.minor_version >= 2;
        conn.xfixes_query_version(5, 0)
            .map_err(|e| x_err("XFixes query", e))?
            .reply()
            .map_err(|e| x_err("the server has no XFixes", e))?;
        let formats = conn
            .render_query_pict_formats()
            .map_err(|e| x_err("Render query", e))?
            .reply()
            .map_err(|e| x_err("the server has no Render extension", e))?
            .formats;
        let direct =
            |f: &render::Pictforminfo, d: u8| f.type_ == render::PictType::DIRECT && f.depth == d;
        let rgb_format = formats
            .iter()
            .find(|f| direct(f, depth) && f.direct.red_shift == 16 && f.direct.blue_shift == 0)
            .map(|f| f.id)
            .ok_or_else(|| format!("no Render format for the depth-{depth} screen"))?;
        let argb_format = formats
            .iter()
            .find(|f| direct(f, 32) && f.direct.alpha_mask == 0xff && f.direct.red_shift == 16)
            .map(|f| f.id)
            .ok_or("no premultiplied ARGB32 Render format for the cursor")?;

        let opened = conn
            .dri3_open(root, 0)
            .map_err(|e| x_err("DRI3 open", e))?
            .reply()
            .map_err(|e| x_err("the server refused DRI3 open", e))?;
        let device_fd = opened.device_fd;
        let encode_path = format!("/dev/dri/renderD{}", 128 + node);
        let encode_dev = std::fs::metadata(&encode_path)
            .map(|m| m.rdev())
            .map_err(|e| x_err(&format!("encode node {encode_path}"), e))?;
        let server_dev = device_of_fd(&device_fd)?;
        if !same_gpu(server_dev, encode_dev) {
            return Err(format!(
                "the X server draws on DRM device {}:{}, not on the GPU of encode node {encode_path}",
                libc::major(server_dev),
                libc::minor(server_dev)
            ));
        }

        let mut modifiers = Vec::new();
        if explicit {
            let modifiers_reply = conn
                .dri3_get_supported_modifiers(root, depth, 32)
                .map_err(|e| x_err("DRI3 modifiers", e))?
                .reply()
                .map_err(|e| x_err("the server listed no buffer modifiers", e))?;
            modifiers = if modifiers_reply.window_modifiers.is_empty() {
                modifiers_reply.screen_modifiers
            } else {
                modifiers_reply.window_modifiers
            };
            modifiers.retain(|&m| !intel_compressed(m));
        }

        let gc = conn.generate_id().map_err(|e| x_err("GC id", e))?;
        conn.create_gc(
            gc,
            root,
            &xproto::CreateGCAux::new()
                .subwindow_mode(xproto::SubwindowMode::INCLUDE_INFERIORS)
                .graphics_exposures(0),
        )
        .map_err(|e| x_err("CreateGC", e))?
        .check()
        .map_err(|e| x_err("CreateGC", e))?;
        let damage = RootDamage::create(&conn, root).ok_or("the server has no Damage extension")?;
        let semaphore_opcode = conn
            .extension_information(SEMAPHORE_EXTENSION)
            .ok()
            .flatten()
            .map(|e| e.major_opcode);

        Ok((
            Self {
                conn,
                root,
                depth,
                byte_order,
                gc,
                damage,
                rgb_format,
                argb_format,
                modifiers,
                explicit,
                device: encode_path,
                semaphore_opcode,
            },
            device_fd,
        ))
    }

    /// One SELKIES-SEMAPHORE request: its minor opcode and CARD32 fields, and an fd if it takes one.
    fn semaphore_request(
        &self,
        minor: u8,
        fields: &[u32],
        fds: Vec<OwnedFd>,
    ) -> Result<x11rb::cookie::VoidCookie<'_, RustConnection>, String> {
        let opcode = self
            .semaphore_opcode
            .ok_or("the server has no SELKIES-SEMAPHORE")?;
        let mut req = vec![opcode, minor];
        req.extend_from_slice(&(1 + fields.len() as u16).to_ne_bytes());
        for f in fields {
            req.extend_from_slice(&f.to_ne_bytes());
        }
        self.conn
            .send_request_without_reply(&[IoSlice::new(&req)], fds)
            .map_err(|e| x_err("SELKIES-SEMAPHORE", e))
    }

    /// Hand the server a semaphore to signal after the blits it is asked to (`signal_semaphore`),
    /// returning its id there.
    fn import_semaphore(&self, fd: OwnedFd) -> Result<u32, String> {
        let id = self
            .conn
            .generate_id()
            .map_err(|e| x_err("semaphore id", e))?;
        self.semaphore_request(1, &[id, self.root], vec![fd])?
            .check()
            .map_err(|e| format!("the server did not take the semaphore: {e}"))?;
        Ok(id)
    }

    /// Have the server signal the semaphore once the rendering requested before has landed.
    /// The check's round trip answers once the server has queued the signal, which is what the
    /// encoder's wait has to come after; an error means no signal is coming.
    fn signal_semaphore(&self, id: u32) -> Result<(), String> {
        self.semaphore_request(2, &[id], Vec::new())?
            .check()
            .map_err(|e| format!("the server did not signal the semaphore: {e}"))
    }

    fn free_semaphore(&self, id: u32) {
        let _ = self.semaphore_request(3, &[id], Vec::new());
    }

    /// Wait for the blit into `pixmap` to land: the reply to a read of one of its pixels comes
    /// back only after the server's GPU work ordered before it has completed.
    fn await_blit(&self, pixmap: xproto::Pixmap) -> Result<(), String> {
        self.conn
            .get_image(ImageFormat::Z_PIXMAP, pixmap, 0, 0, 1, 1, !0u32)
            .map_err(|e| x_err("blit sync", e))?
            .reply()
            .map_err(|e| x_err("blit sync reply", e))?;
        Ok(())
    }

    /// Hand the server an ARGB image as a pixmap it can composite from. `argb` is one
    /// premultiplied 0xAARRGGBB word per pixel, which is what Render's OVER expects.
    fn upload_sprite(&self, argb: &[u32], width: u16, height: u16) -> Result<Sprite, String> {
        let conn = &self.conn;
        let pixmap = conn
            .generate_id()
            .map_err(|e| x_err("sprite pixmap id", e))?;
        conn.create_pixmap(32, pixmap, self.root, width, height)
            .map_err(|e| x_err("sprite CreatePixmap", e))?;
        let gc = conn.generate_id().map_err(|e| x_err("sprite GC id", e))?;
        conn.create_gc(
            gc,
            pixmap,
            &xproto::CreateGCAux::new().graphics_exposures(0),
        )
        .map_err(|e| x_err("sprite CreateGC", e))?;
        let picture = conn
            .generate_id()
            .map_err(|e| x_err("sprite picture id", e))?;
        conn.render_create_picture(
            picture,
            pixmap,
            self.argb_format,
            &render::CreatePictureAux::new(),
        )
        .map_err(|e| x_err("sprite CreatePicture", e))?;
        let mut data = Vec::with_capacity(argb.len() * 4);
        for px in argb {
            let bytes = if self.byte_order == ImageOrder::LSB_FIRST {
                px.to_le_bytes()
            } else {
                px.to_be_bytes()
            };
            data.extend_from_slice(&bytes);
        }
        conn.put_image(
            ImageFormat::Z_PIXMAP,
            pixmap,
            gc,
            width,
            height,
            0,
            0,
            0,
            32,
            &data,
        )
        .map_err(|e| x_err("sprite PutImage", e))?;
        Ok(Sprite {
            pixmap,
            picture,
            gc,
            width,
            height,
        })
    }

    fn free_sprite(&self, s: &Sprite) {
        let _ = self.conn.render_free_picture(s.picture);
        let _ = self.conn.free_gc(s.gc);
        let _ = self.conn.free_pixmap(s.pixmap);
    }

    fn free_buffer(&self, b: &GpuBuffer) {
        let _ = self.conn.render_free_picture(b.picture);
        let _ = self.conn.free_pixmap(b.pixmap);
    }

    fn free_cursor(&self, c: &CursorSprite) {
        let _ = self.conn.render_free_picture(c.picture);
        let _ = self.conn.free_gc(c.gc);
        let _ = self.conn.free_pixmap(c.pixmap);
    }
}

impl GpuCapture {
    /// One buffer of the pool: a single-plane allocation the server and the encoder both
    /// describe with one fd, stride, and offset. A modifier that spreads the buffer over
    /// auxiliary planes (a compressed layout) is dropped from the server's list and the
    /// allocation retried, down to the driver's own choice.
    fn alloc_bo(&mut self, w: u16, h: u16) -> Result<BufferObject<()>, String> {
        if !self.x.explicit {
            // Linear as the one modifier allowed first: NVIDIA's GBM takes that and refuses the
            // LINEAR usage flag, which Mesa's takes.
            let by_modifier = self.gbm.create_buffer_object_with_modifiers::<()>(
                w as u32,
                h as u32,
                GbmFormat::Argb8888,
                std::iter::once(gbm::Modifier::Linear),
            );
            return match by_modifier {
                Ok(bo) => Ok(bo),
                Err(first) => self
                    .gbm
                    .create_buffer_object::<()>(
                        w as u32,
                        h as u32,
                        GbmFormat::Argb8888,
                        gbm::BufferObjectFlags::RENDERING | gbm::BufferObjectFlags::LINEAR,
                    )
                    .map_err(|e| {
                        format!(
                            "linear GBM allocation {w}x{h}: {first:?} by modifier, {e:?} by flag"
                        )
                    }),
            };
        }
        loop {
            let bo = if self.x.modifiers.is_empty() {
                crate::alloc_render_target(&self.gbm, w as u32, h as u32, GbmFormat::Argb8888)
            } else {
                // The entry point without the flags argument, which implies exactly the
                // rendering use this asks for: the one that takes flags arrived in Mesa 21.1
                // and a wheel referencing it does not load at all against an older libgbm,
                // since Python resolves an extension's symbols eagerly.
                self.gbm.create_buffer_object_with_modifiers::<()>(
                    w as u32,
                    h as u32,
                    GbmFormat::Argb8888,
                    self.x.modifiers.iter().map(|&m| gbm::Modifier::from(m)),
                )
            };
            let bo = match bo {
                Ok(bo) => bo,
                Err(e) if !self.x.modifiers.is_empty() => {
                    crate::log::debug!(
                        "[X11] DRI3 capture: allocation with the server's modifiers failed ({e:?}); trying without"
                    );
                    self.x.modifiers.clear();
                    continue;
                }
                Err(e) => return Err(format!("GBM allocation {w}x{h}: {e:?}")),
            };
            if bo.plane_count() <= 1 {
                return Ok(bo);
            }
            let chosen: u64 = bo.modifier().into();
            if self.x.modifiers.is_empty() {
                return Err(format!(
                    "the driver lays {w}x{h} out over {} planes even unconstrained",
                    bo.plane_count()
                ));
            }
            crate::log::debug!(
                "[X11] DRI3 capture: modifier {chosen:#x} lays the buffer out in {} planes; trying another",
                bo.plane_count()
            );
            drop(bo);
            prune_modifier(&mut self.x.modifiers, chosen);
        }
    }

    /// Allocate the buffer pool at `w` x `h` and hand each buffer to the server as a pixmap.
    fn alloc_buffers(&mut self, w: u16, h: u16) -> Result<(), String> {
        let mut fresh = Vec::with_capacity(POOL_N);
        for _ in 0..POOL_N {
            let bo = self.alloc_bo(w, h)?;
            let dmabuf = crate::create_dmabuf_from_bo(&bo);
            let server_fd = bo
                .fd()
                .map_err(|e| format!("dmabuf fd for the server: {e:?}"))?;
            let modifier: u64 = bo.modifier().into();
            let pixmap = self
                .x
                .conn
                .generate_id()
                .map_err(|e| x_err("pixmap id", e))?;
            if self.x.explicit {
                self.x
                    .conn
                    .dri3_pixmap_from_buffers(
                        pixmap,
                        self.x.root,
                        w,
                        h,
                        bo.stride(),
                        0,
                        0,
                        0,
                        0,
                        0,
                        0,
                        0,
                        self.x.depth,
                        32,
                        modifier,
                        vec![server_fd],
                    )
                    .map_err(|e| x_err("PixmapFromBuffers", e))?
                    .check()
                    .map_err(|e| {
                        format!(
                            "the server did not import the buffer (modifier {modifier:#x}): {e}"
                        )
                    })?;
            } else {
                // DRI3 1.0: one plane, its stride, and no layout but the linear one.
                let stride = u16::try_from(bo.stride())
                    .map_err(|_| format!("a {w}-pixel row's stride overflows DRI3 1.0"))?;
                self.x
                    .conn
                    .dri3_pixmap_from_buffer(
                        pixmap,
                        self.x.root,
                        bo.stride() * h as u32,
                        w,
                        h,
                        stride,
                        self.x.depth,
                        32,
                        server_fd,
                    )
                    .map_err(|e| x_err("PixmapFromBuffer", e))?
                    .check()
                    .map_err(|e| format!("the server did not import the linear buffer: {e}"))?;
            }
            let picture = self
                .x
                .conn
                .generate_id()
                .map_err(|e| x_err("picture id", e))?;
            self.x
                .conn
                .render_create_picture(
                    picture,
                    pixmap,
                    self.x.rgb_format,
                    &render::CreatePictureAux::new(),
                )
                .map_err(|e| x_err("CreatePicture", e))?
                .check()
                .map_err(|e| x_err("CreatePicture", e))?;
            fresh.push(GpuBuffer {
                _bo: bo,
                dmabuf,
                pixmap,
                picture,
            });
        }
        self.free_buffers();
        self.buffers = fresh;
        self.next = 0;
        Ok(())
    }

    fn free_buffers(&mut self) {
        for b in self.buffers.drain(..) {
            self.x.free_buffer(&b);
        }
    }

    /// Bring the pool and the encoder to a new capture size. NVENC follows in place; a session
    /// that cannot is rebuilt through the same ladder that chose it.
    fn reshape(&mut self, w: u16, h: u16) -> Result<(), String> {
        self.alloc_buffers(w, h)?;
        self.settings.width = w as i32;
        self.settings.height = h as i32;
        let in_place = match self.encoder.as_mut() {
            Some(FrameEncoder::Nvenc(enc)) => {
                enc.reconfigure_resolution(&self.settings).map(|_| ())
            }
            Some(FrameEncoder::Vaapi(_)) => {
                Err("a VA-API session is rebuilt at a new size".to_string())
            }
            Some(FrameEncoder::Vpx(_) | FrameEncoder::Hevc(_) | FrameEncoder::Av1(_)) => {
                Err("a software session takes host frames".to_string())
            }
            // Neither a Tegra nor a V4L2 M2M session reaches this path: both take host frames,
            // so `open` below declines the zero-copy capture before one is built. The arms exist
            // because the variants do, saying what would happen rather than panicking.
            #[cfg(target_arch = "aarch64")]
            Some(FrameEncoder::Tegra(_)) => Err("a Tegra session takes host frames".to_string()),
            Some(FrameEncoder::V4l2m2m(_)) => {
                Err("a V4L2 M2M session takes host frames".to_string())
            }
            None => Err("no session to reconfigure".to_string()),
        };
        if let Err(e) = in_place {
            crate::log::debug!("[X11] DRI3 capture: {e}");
            self.rebuild_encoder()?;
        }
        Ok(())
    }

    fn enc(&mut self) -> &mut FrameEncoder {
        self.encoder
            .as_mut()
            .expect("the DRI3 capture always carries an encoder between frames")
    }

    /// The encoder's name for the logs.
    fn backend(&self) -> &'static str {
        self.encoder
            .as_ref()
            .map(|e| e.backend_name())
            .unwrap_or("none")
    }

    /// Replace the encoder with the startup construction, releasing the old session first so
    /// the two never hold device memory at once.
    fn rebuild_encoder(&mut self) -> Result<(), String> {
        self.detach_semaphore();
        drop(self.encoder.take());
        let source = FrameSource::Dmabuf {
            egl_display: self.egl_display,
        };
        let mut settings = self.settings.clone();
        match encoders::select_frame_encoder(&mut settings, source, None, "X11") {
            Some(enc) if enc.is_hardware() => {
                self.encoder = Some(enc);
                self.settings = settings;
                self.attach_semaphore();
                Ok(())
            }
            Some(_) => Err("no hardware encoder is left for the zero-copy path".to_string()),
            None => Err("no encoder could be rebuilt for the zero-copy path".to_string()),
        }
    }

    /// Give the server the encoder's blit semaphore, where it offers SELKIES-SEMAPHORE and the
    /// encoder can wait on one; otherwise each blit is waited for with a GetImage.
    fn attach_semaphore(&mut self) {
        self.detach_semaphore();
        if self.x.semaphore_opcode.is_none() {
            return;
        }
        let Some(fd) = self.enc().blit_semaphore_fd() else {
            return;
        };
        match fd.and_then(|fd| self.x.import_semaphore(fd)) {
            Ok(id) => {
                self.blit_semaphore = Some(id);
                println!(
                    "[X11] DRI3 capture: the encoder waits for each blit on the GPU, behind a semaphore the server signals ({SEMAPHORE_EXTENSION})."
                );
            }
            Err(e) => crate::log::debug!(
                "[X11] DRI3 capture: no blit semaphore ({e}); each blit is waited for with a GetImage"
            ),
        }
    }

    fn detach_semaphore(&mut self) {
        if let Some(id) = self.blit_semaphore.take() {
            self.x.free_semaphore(id);
        }
    }

    /// Blit the capture region into the next pool buffer, composite the cursor if asked, and
    /// either have the server signal the blit semaphore after it or wait for it to land. Returns
    /// the buffer to encode and whether its encode has to wait on the semaphore.
    fn grab(&mut self, with_cursor: bool) -> Result<(usize, bool), String> {
        let idx = self.next;
        self.next = (self.next + 1) % self.buffers.len();
        let (w, h) = (self.settings.width as u16, self.settings.height as u16);
        let dst = self.buffers[idx].pixmap;
        self.x
            .conn
            .copy_area(
                self.x.root,
                dst,
                self.x.gc,
                self.cap_x,
                self.cap_y,
                0,
                0,
                w,
                h,
            )
            .map_err(|e| x_err("CopyArea", e))?;
        if with_cursor {
            self.composite_cursor(idx)?;
        } else if let Some(c) = self.cursor.take() {
            self.x.free_cursor(&c);
        }
        self.composite_watermark(idx)?;
        if let Some(id) = self.blit_semaphore {
            match self.x.signal_semaphore(id) {
                Ok(()) => return Ok((idx, true)),
                // No signal is coming, so this frame waits for its blit the other way, and so
                // does every later one.
                Err(e) => {
                    eprintln!(
                        "[X11] DRI3 capture: {e}; waiting for each blit with a GetImage from now on."
                    );
                    self.detach_semaphore();
                }
            }
        }
        self.x.await_blit(dst)?;
        Ok((idx, false))
    }

    /// Draw the watermark over the freshly blitted frame, on the GPU. The image is uploaded
    /// on its first frame and composited by the server after that, so the CPU never sees the
    /// pixels it is drawn on; an animated location only moves where it is composited.
    fn composite_watermark(&mut self, idx: usize) -> Result<(), String> {
        let Some((pixels, w, h)) = self.overlay.sprite() else {
            return Ok(());
        };
        if self.watermark.is_none() {
            // `image` decodes to straight alpha and Render's OVER wants it premultiplied.
            let argb: Vec<u32> = pixels
                .as_chunks::<4>()
                .0
                .iter()
                .map(|p| {
                    let a = p[3] as u32;
                    let pm = |c: u8| (c as u32 * a / 255) & 0xff;
                    (a << 24) | (pm(p[0]) << 16) | (pm(p[1]) << 8) | pm(p[2])
                })
                .collect();
            self.watermark = Some(self.x.upload_sprite(&argb, w as u16, h as u16)?);
        }
        let (fw, fh) = (self.settings.width, self.settings.height);
        self.overlay
            .update_position(fw, fh, self.request.watermark_location_enum);
        let (x, y) = self.overlay.position();
        let sprite = self.watermark.as_ref().unwrap();
        if x >= fw || y >= fh || x + sprite.width as i32 <= 0 || y + sprite.height as i32 <= 0 {
            return Ok(());
        }
        self.x
            .conn
            .render_composite(
                render::PictOp::OVER,
                sprite.picture,
                x11rb::NONE,
                self.buffers[idx].picture,
                0,
                0,
                0,
                0,
                x.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                y.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                sprite.width,
                sprite.height,
            )
            .map_err(|e| x_err("watermark Composite", e))?;
        Ok(())
    }

    /// Draw the XFixes cursor over the freshly blitted frame, uploading its image only when the
    /// cursor changed. The server paints it through Render, on the GPU.
    fn composite_cursor(&mut self, idx: usize) -> Result<(), String> {
        let Some(c) = self
            .x
            .conn
            .xfixes_get_cursor_image()
            .ok()
            .and_then(|c| c.reply().ok())
        else {
            return Ok(());
        };
        if c.width == 0 || c.height == 0 {
            return Ok(());
        }
        let stale = self.cursor.as_ref().is_none_or(|s| {
            s.serial != c.cursor_serial || s.width != c.width || s.height != c.height
        });
        if stale {
            if let Some(old) = self.cursor.take() {
                self.x.free_cursor(&old);
            }
            let conn = &self.x.conn;
            let pixmap = conn
                .generate_id()
                .map_err(|e| x_err("cursor pixmap id", e))?;
            conn.create_pixmap(32, pixmap, self.x.root, c.width, c.height)
                .map_err(|e| x_err("cursor CreatePixmap", e))?;
            let gc = conn.generate_id().map_err(|e| x_err("cursor GC id", e))?;
            conn.create_gc(
                gc,
                pixmap,
                &xproto::CreateGCAux::new().graphics_exposures(0),
            )
            .map_err(|e| x_err("cursor CreateGC", e))?;
            let picture = conn
                .generate_id()
                .map_err(|e| x_err("cursor picture id", e))?;
            conn.render_create_picture(
                picture,
                pixmap,
                self.x.argb_format,
                &render::CreatePictureAux::new(),
            )
            .map_err(|e| x_err("cursor CreatePicture", e))?;
            let mut data = Vec::with_capacity(c.cursor_image.len() * 4);
            for px in &c.cursor_image {
                let bytes = if self.x.byte_order == ImageOrder::LSB_FIRST {
                    px.to_le_bytes()
                } else {
                    px.to_be_bytes()
                };
                data.extend_from_slice(&bytes);
            }
            conn.put_image(
                ImageFormat::Z_PIXMAP,
                pixmap,
                gc,
                c.width,
                c.height,
                0,
                0,
                0,
                32,
                &data,
            )
            .map_err(|e| x_err("cursor PutImage", e))?;
            self.cursor = Some(CursorSprite {
                pixmap,
                picture,
                gc,
                width: c.width,
                height: c.height,
                serial: c.cursor_serial,
            });
        }
        let sprite = self.cursor.as_ref().unwrap();
        let (img_x, img_y) = cursor_image_origin(
            c.x,
            c.y,
            c.xhot,
            c.yhot,
            self.cap_x as i32,
            self.cap_y as i32,
        );
        let (fw, fh) = (self.settings.width, self.settings.height);
        if img_x >= fw || img_y >= fh || img_x + c.width as i32 <= 0 || img_y + c.height as i32 <= 0
        {
            return Ok(());
        }
        self.x
            .conn
            .render_composite(
                render::PictOp::OVER,
                sprite.picture,
                x11rb::NONE,
                self.buffers[idx].picture,
                0,
                0,
                0,
                0,
                img_x.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                img_y.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                sprite.width,
                sprite.height,
            )
            .map_err(|e| x_err("cursor Composite", e))?;
        Ok(())
    }
}

impl Drop for GpuCapture {
    fn drop(&mut self) {
        self.detach_semaphore();
        if let Some(c) = self.cursor.take() {
            self.x.free_cursor(&c);
        }
        if let Some(w) = self.watermark.take() {
            self.x.free_sprite(&w);
        }
        self.free_buffers();
        let _ = self.x.conn.free_gc(self.x.gc);
        let _ = self.x.conn.flush();
    }
}

/// Build the zero-copy capture for these settings, or report why it cannot serve them.
///
/// Everything fallible happens here, before a single frame is delivered, so the caller can fall
/// back to the XShm path with nothing half-built. The server is asked first, over a connection
/// of our own, because whether it draws on the GPU at all, and on which device, is its answer to
/// give; the encoder is built on the device the server named.
fn open(settings: &CaptureSettings) -> Option<GpuCapture> {
    if !settings.codec.is_video() {
        return declined("the codec is JPEG");
    }
    if settings.use_cpu || settings.encode_node_index == -1 {
        return declined("software encoding was requested");
    }
    let node = settings.encode_node_index.max(0);
    let driver = crate::get_gpu_driver(node);
    if driver.is_empty() {
        return declined(&format!("encode node {node} has no readable render node"));
    }
    let (x, device_fd) = match XScreen::open(node) {
        Ok(v) => v,
        Err(e) => return declined(&e),
    };
    crate::report::zero_copy_available(true);

    let alloc_file = File::from(device_fd);
    let egl_file = match alloc_file.try_clone() {
        Ok(f) => f,
        Err(e) => return declined(&format!("dup of the DRI3 device: {e}")),
    };
    let gbm = match RawGbmDevice::new(alloc_file) {
        Ok(d) => d,
        Err(e) => return declined(&format!("no GBM device on {}: {e:?}", x.device)),
    };
    // NVENC imports through an EGL display on the same device; VA-API needs none.
    let wants_egl = crate::driver_selects_nvenc(&driver);
    let egl = GbmDevice::new(egl_file)
        .ok()
        .and_then(|dev| unsafe { EGLDisplay::new(dev) }.ok());
    if wants_egl && egl.is_none() {
        return declined(&format!(
            "no EGL display on {} for the NVENC import",
            x.device
        ));
    }
    let egl_display = egl
        .as_ref()
        .map(|e| e.get_display_handle().handle)
        .unwrap_or(std::ptr::null());

    let geo = match x
        .conn
        .get_geometry(x.root)
        .ok()
        .and_then(|c| c.reply().ok())
    {
        Some(g) => g,
        None => return declined("the root geometry could not be read"),
    };
    let mut watermark = crate::encoders::overlay::OverlayState::default();
    if !settings.watermark_path.is_empty() {
        watermark.load_watermark(&settings.watermark_path, 1.0);
    }
    let request = settings.clone();
    let mut live = settings.clone();
    let (w, h) = resolve_dims(geo.width, geo.height, &request);
    live.width = w as i32;
    live.height = h as i32;

    let mut gpu = GpuCapture {
        encoder: None,
        buffers: Vec::new(),
        cursor: None,
        overlay: watermark,
        watermark: None,
        cap_x: clamp_offset(request.capture_x, geo.width),
        cap_y: clamp_offset(request.capture_y, geo.height),
        root_w: geo.width,
        root_h: geo.height,
        blit_semaphore: None,
        x,
        gbm,
        _egl: egl,
        egl_display,
        next: 0,
        settings: live,
        request,
    };
    if let Err(e) = gpu.alloc_buffers(w, h) {
        return declined(&e);
    }
    let mut live = gpu.settings.clone();
    gpu.encoder = match encoders::select_frame_encoder(
        &mut live,
        FrameSource::Dmabuf { egl_display },
        None,
        "X11",
    ) {
        Some(enc) if enc.is_hardware() => Some(enc),
        Some(_) => return declined("the session encodes in software"),
        None => return declined("no hardware encoder opened for this session"),
    };
    gpu.settings = live;
    gpu.attach_semaphore();
    println!(
        "[X11] Zero-copy capture (DRI3): {}x{} blitted by the X server into {} GPU buffers on {}, encoded in place on {}.",
        gpu.settings.width,
        gpu.settings.height,
        POOL_N,
        gpu.x.device,
        gpu.backend()
    );
    crate::report::capture("DRI3", true);
    crate::log_stream_settings("X11", &gpu.settings, 1, gpu.encoder.as_ref());
    Some(gpu)
}

/// Run the zero-copy X11 capture until `controls.stop` is set, or report that it cannot start.
///
/// `None` means DRI3 cannot serve this session and nothing was delivered, which is the caller's
/// signal to run the XShm path instead. That includes a first frame the encoder could not read,
/// since a buffer the server imported is not yet proof the encoder can. Anything else means the
/// capture ran, every frame going from the server's blit into the encoder without a copy, and
/// carries how it ended.
///
/// Each iteration paces to the live target rate, applies the cross-thread controls on the thread
/// that owns the sessions, follows the root geometry, asks the server whether anything was drawn,
/// and only then blits, waits, and encodes through the same send / quality / key-frame policy every
/// full-frame encoder obeys.
pub fn run_capture<F>(
    settings: CaptureSettings,
    controls: Arc<Controls>,
    encode_tid_tx: std::sync::mpsc::Sender<std::thread::ThreadId>,
    on_frame: &mut F,
) -> Option<Result<(), String>>
where
    F: FnMut(Vec<EncodedStripe>),
{
    let mut gpu = open(&settings)?;
    controls
        .codec
        .store(gpu.enc().codec().id(), Ordering::Relaxed);

    let recording_sink = RecordingSink::try_bind(&settings.recording_socket, settings.target_fps);
    let mut state = StripeState::default();
    let mut frame_counter: u16 = 0;
    let mut pending_force_idr = true;
    // One frame a period at most, a change taking its period's frame as it lands (FramePace::windowed).
    let mut pace = FramePace::windowed();
    let mut geometry_check = GEOMETRY_POLL_FRAMES;
    let mut x_failures = 0u32;
    let mut encode_errors = 0u32;
    let mut encoder_rebuilt = false;
    // Capture and encode share this thread, so it is what the re-entrant-stop guard watches;
    // it is named only once the path is committed, because a decline on the first frame hands
    // the session to XShm, whose own encode thread must then be the one on the channel.
    let mut delivered_any = false;
    let mut last_log = Instant::now();
    let mut sent_frames: u64 = 0;
    let mut damaged_frames: u64 = 0;
    let mut first_frame = true;
    let mut tid_sent = false;

    while !controls.stop.load(Ordering::Relaxed) {
        let fps = f64::from_bits(controls.fps_bits.load(Ordering::Relaxed)).max(1.0);
        gpu.settings.video_link_kbps = controls.link_kbps.load(Ordering::Relaxed);
        let frame_dur = Duration::from_secs_f64(1.0 / fps.max(1.0));
        let trigger = wait_for_frame(&gpu.x.conn, Some(&gpu.x.damage), &pace, frame_dur);
        let woke = Instant::now();
        // The report is spent where it is read, before the blit, so a change racing the blit
        // wakes the next frame rather than being cleared along with what the blit captured.
        let (dirty, changed) = take_tick(&gpu.x.damage, &gpu.x.conn, trigger);
        let is_dirty = dirty || first_frame;
        // Where the root changed, None where the server cannot say or the frame is the first.
        let changed = changed.filter(|rects| !first_frame && !rects.is_empty());
        first_frame = false;
        damaged_frames += is_dirty as u64;
        if controls.stop.load(Ordering::Relaxed) {
            break;
        }

        for report in std::mem::take(&mut *controls.reference_reports.lock().unwrap()) {
            state.take_report(report);
            if !gpu.enc().take_report(report) {
                pending_force_idr = true;
            }
        }
        if controls.force_idr.swap(false, Ordering::Relaxed)
            || recording_sink
                .as_ref()
                .is_some_and(|s| s.should_force_idr())
        {
            pending_force_idr = true;
        }
        if controls.rate_dirty.swap(false, Ordering::Acquire) {
            gpu.settings.video_bitrate_kbps = controls.bitrate_kbps.load(Ordering::Relaxed);
            gpu.settings.video_vbv_multiplier =
                controls.vbv_mult_milli.load(Ordering::Relaxed) as f64 / 1000.0;
            gpu.settings.target_fps =
                f64::from_bits(controls.fps_bits.load(Ordering::Relaxed)).max(1.0);
            let live = gpu.settings.clone();
            if let Err(e) = gpu.enc().reconfigure_rate(&live) {
                eprintln!(
                    "[X11] DRI3 capture: rate reconfigure failed ({e}); rebuilding the encoder."
                );
                if let Err(e) = gpu.rebuild_encoder() {
                    return Some(Err(format!("DRI3 capture ended: {e}")));
                }
                state = StripeState::default();
                pending_force_idr = true;
            }
        }
        if controls.tunables_dirty.swap(false, Ordering::Acquire)
            && let Some(t) = controls.tunables.lock().unwrap().take()
        {
            t.apply_to(&mut gpu.settings);
        }

        let want_cursor = controls.capture_cursor.load(Ordering::Relaxed);
        let mut recheck_geometry = controls.region_dirty.swap(false, Ordering::Acquire);
        if recheck_geometry {
            let (nx, ny, nw, nh) = *controls.region.lock().unwrap();
            gpu.request.capture_x = nx;
            gpu.request.capture_y = ny;
            gpu.request.width = nw;
            gpu.request.height = nh;
            gpu.request.auto_adjust_screen_capture_size = nw <= 0 || nh <= 0;
        }
        geometry_check -= 1;
        if geometry_check <= 0 {
            geometry_check = GEOMETRY_POLL_FRAMES;
            recheck_geometry = true;
        }
        if recheck_geometry {
            match gpu
                .x
                .conn
                .get_geometry(gpu.x.root)
                .ok()
                .and_then(|c| c.reply().ok())
            {
                Some(g) => {
                    gpu.root_w = g.width;
                    gpu.root_h = g.height;
                }
                None => {
                    x_failures += 1;
                    if x_failures > MAX_X_FAILURES {
                        return Some(Err(
                            "DRI3 capture ended: the X server stopped answering".to_string()
                        ));
                    }
                    continue;
                }
            }
            gpu.cap_x = clamp_offset(gpu.request.capture_x, gpu.root_w);
            gpu.cap_y = clamp_offset(gpu.request.capture_y, gpu.root_h);
            let (w, h) = resolve_dims(gpu.root_w, gpu.root_h, &gpu.request);
            if w as i32 != gpu.settings.width || h as i32 != gpu.settings.height {
                if let Err(e) = gpu.reshape(w, h) {
                    return Some(Err(format!(
                        "DRI3 capture could not follow the new geometry: {e}"
                    )));
                }
                state = StripeState::default();
                pending_force_idr = true;
            }
        }

        if gpu.enc().take_lost_cleanup() {
            state.lost_frame();
        }
        let quality = EncoderQuality::of(gpu.enc());
        let decision = decide_hw_fullframe(
            &mut state,
            &gpu.settings,
            frame_counter,
            match changed {
                _ if !is_dirty => Damage::None,
                Some(rects) => captured_damage(
                    &rects,
                    gpu.cap_x.into(),
                    gpu.cap_y.into(),
                    gpu.settings.width,
                    gpu.settings.height,
                ),
                None => Damage::Unknown,
            },
            false,
            pending_force_idr,
            quality,
        );
        // A tick that publishes nothing is no frame: counted as one, it would hold a change
        // landing just after it back by a pull's worth of budget or a whole period.
        pace.record_tick(trigger, frame_dur, woke, decision.send);
        let mut delivered = false;
        if decision.send {
            // The frame is the screen as the CopyArea finds it, so it is stamped as that is sent:
            // a later reading would leave out the wait for the blit, which is most of a frame's
            // latency under a GPU-bound client.
            let grabbed_ns = crate::wayland::host::now_ns();
            let (idx, after_blit) = match gpu.grab(want_cursor) {
                Ok(i) => {
                    x_failures = 0;
                    i
                }
                Err(e) => {
                    x_failures += 1;
                    if x_failures > MAX_X_FAILURES {
                        return Some(Err(format!("DRI3 capture ended: {e}")));
                    }
                    frame_counter = frame_counter.wrapping_add(1);
                    continue;
                }
            };
            let dmabuf = gpu.buffers[idx].dmabuf.clone();
            decision.prepare(gpu.enc());
            let encode_start_ns = crate::wayland::host::now_ns();
            let result = if after_blit {
                gpu.enc().encode_dmabuf_after_blit(
                    &dmabuf,
                    frame_counter as u64,
                    decision.target_qp,
                    decision.force_idr,
                )
            } else {
                gpu.enc().encode_dmabuf(
                    &dmabuf,
                    frame_counter as u64,
                    decision.target_qp,
                    decision.force_idr,
                )
            };
            match result {
                Ok(data) if !data.is_empty() => {
                    encode_errors = 0;
                    let stripes = vec![EncodedStripe {
                        data: Arc::new(data),
                        codec: gpu.settings.codec,
                        stripe_y_start: 0,
                        stripe_height: gpu.settings.height,
                        frame_id: frame_counter as i32,
                        timing: FrameTiming {
                            capture_ns: grabbed_ns,
                            encode_start_ns,
                            encode_end_ns: crate::wayland::host::now_ns(),
                        },
                        reference: gpu.enc().last_reference(),
                    }];
                    if let Some(sink) = &recording_sink {
                        sink.write_frame(&stripes, gpu.settings.width, gpu.settings.height);
                    }
                    sent_frames += 1;
                    delivered = true;
                    delivered_any = true;
                    on_frame(stripes);
                }
                Ok(_) => {
                    encode_errors = 0;
                    delivered_any = true;
                }
                Err(e) => {
                    if !delivered_any {
                        // Nothing has reached a client, so this is still the time to decline.
                        drop(gpu);
                        declined(&format!(
                            "the encoder could not read the server's buffer: {e}"
                        ));
                        return None;
                    }
                    encode_errors += 1;
                    if encode_errors % crate::HW_ERROR_RECOVERY_THRESHOLD == 1 {
                        eprintln!("[X11] hardware encode error on the DRI3 path: {e}");
                    }
                    if encode_errors >= crate::HW_ERROR_RECOVERY_THRESHOLD {
                        if encoder_rebuilt {
                            return Some(Err(
                                "the encoder failed repeatedly on the DRI3 path".to_string()
                            ));
                        }
                        eprintln!(
                            "[X11] rebuilding the encoder after repeated errors on the DRI3 path."
                        );
                        if let Err(e) = gpu.rebuild_encoder() {
                            return Some(Err(format!("DRI3 capture ended: {e}")));
                        }
                        encoder_rebuilt = true;
                        encode_errors = 0;
                        state = StripeState::default();
                        pending_force_idr = true;
                    }
                }
            }
        }
        if delivered_any && !tid_sent {
            let _ = encode_tid_tx.send(std::thread::current().id());
            tid_sent = true;
        }
        pending_force_idr = (pending_force_idr || decision.force_idr) && !delivered;
        frame_counter = frame_counter.wrapping_add(1);

        let elapsed = last_log.elapsed().as_secs_f64();
        if elapsed >= 1.0 {
            crate::log::debug!(
                "[X11] DRI3 {}x{} Encoder: {} EncFPS: {:.2} Damaged/s: {:.2}",
                gpu.settings.width,
                gpu.settings.height,
                gpu.backend(),
                sent_frames as f64 / elapsed,
                damaged_frames as f64 / elapsed
            );
            sent_frames = 0;
            damaged_frames = 0;
            last_log = Instant::now();
        }
    }
    Some(Ok(()))
}

#[cfg(test)]
mod modifier_tests {
    use super::{intel_compressed, prune_modifier};

    const CCS_CC: u64 = 0x0100_0000_0000_0008;
    const CCS: u64 = 0x0100_0000_0000_0006;
    const Y_TILED: u64 = 0x0100_0000_0000_0002;

    /// The list Xvfb offers on an Arc card keeps its linear and tiled layouts and loses both
    /// compressed ones, the single-plane one included.
    #[test]
    fn the_single_plane_compressed_layouts_are_not_offered() {
        const X_TILED: u64 = 0x0100_0000_0000_0001;
        const TILE4: u64 = 0x0100_0000_0000_0009;
        const DG2_RC_CCS: u64 = 0x0100_0000_0000_000a;
        const DG2_RC_CCS_CC: u64 = 0x0100_0000_0000_000c;
        let mut mods = vec![0, X_TILED, TILE4, DG2_RC_CCS, DG2_RC_CCS_CC];
        mods.retain(|&m| !intel_compressed(m));
        assert_eq!(mods, vec![0, X_TILED, TILE4]);
        assert!(intel_compressed(CCS) && intel_compressed(CCS_CC));
        assert!(!intel_compressed(Y_TILED) && !intel_compressed(0x00ff_ffff_ffff_ffff));
    }

    /// A compressed layout the server offered is dropped and the rest stays offered, so the
    /// retry walks the server's list down to a single-plane layout.
    #[test]
    fn a_rejected_modifier_leaves_the_others() {
        let mut mods = vec![CCS_CC, CCS, Y_TILED];
        prune_modifier(&mut mods, CCS_CC);
        assert_eq!(mods, vec![CCS, Y_TILED]);
        prune_modifier(&mut mods, CCS);
        assert_eq!(mods, vec![Y_TILED]);
    }

    /// A layout the list never named came from the driver's own choice, so the list is spent
    /// and the next allocation is unconstrained rather than retried against the same list.
    #[test]
    fn a_choice_outside_the_list_spends_it() {
        let mut mods = vec![CCS, Y_TILED];
        prune_modifier(&mut mods, 0x00ff_ffff_ffff_ffff);
        assert!(mods.is_empty());
    }
}

#[cfg(test)]
mod gpu_tests {
    use super::super::gpu_test_support::{decoded_mean, paint_root, painted_ycbcr, settings};
    use super::*;
    use crate::encoders::codec::{FRAME_DELTA, FRAME_KEY, VIDEO_HEADER_LEN, parse_video_type};
    use crate::webcam::decode::{Codec as DecCodec, Decoder, VideoDecoder};

    /// Encode the buffer `grab` returned, behind the blit semaphore where the server signals it,
    /// as the capture does.
    fn encode_grabbed(
        gpu: &mut GpuCapture,
        (idx, after_blit): (usize, bool),
        i: u64,
        key: bool,
    ) -> Vec<u8> {
        let dmabuf = gpu.buffers[idx].dmabuf.clone();
        if after_blit {
            gpu.enc().encode_dmabuf_after_blit(&dmabuf, i, 25, key)
        } else {
            gpu.enc().encode_dmabuf(&dmabuf, i, 25, key)
        }
        .expect("encode in place")
    }

    /// The primary node of the encode node's GPU is the same GPU to the DRI3 check, as the
    /// node an Xorg on the modesetting driver hands its DRI3 clients. Ignored by default.
    #[test]
    #[ignore]
    fn gpu_dri3_counts_the_primary_node_of_the_encode_gpu_as_it() {
        let node = settings(crate::encoders::codec::Codec::H264)
            .encode_node_index
            .max(0);
        let render = format!("/dev/dri/renderD{}", 128 + node);
        let render_dev = std::fs::metadata(&render).expect("encode node").rdev();
        let primary = std::fs::read_dir(format!("/sys/class/drm/renderD{}/device/drm", 128 + node))
            .expect("the encode node's GPU in sysfs")
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .find(|name| name.starts_with("card"))
            .expect("a primary node on the encode node's GPU");
        let primary_dev = std::fs::metadata(format!("/dev/dri/{primary}"))
            .expect("primary node")
            .rdev();
        assert_ne!(
            primary_dev, render_dev,
            "{primary} and {render} are distinct nodes"
        );
        assert!(
            same_gpu(primary_dev, render_dev),
            "{primary} is the GPU of {render}"
        );
    }

    /// The watermark is composited by the server, so it reaches the frame without the CPU
    /// touching a pixel, and a translucent one blends as the readback paths blend it: Render's
    /// OVER takes premultiplied alpha, and a straight-alpha upload would darken it instead.
    /// Ignored by default; same server and encoder as the check below.
    #[test]
    #[ignore]
    fn gpu_dri3_composites_a_watermark_without_a_readback() {
        const SCREEN: (u8, u8, u8) = (0x20, 0x40, 0x80);
        const MARK: (u8, u8, u8) = (0xff, 0x00, 0x00);
        const ALPHA: u8 = 128;
        if !paint_root(SCREEN) {
            println!("no X root this host can paint (xsetroot, $DISPLAY); nothing to capture");
            return;
        }
        let path = std::env::temp_dir().join("pixelflux-dri3-watermark.png");
        image::RgbaImage::from_pixel(96, 96, image::Rgba([MARK.0, MARK.1, MARK.2, ALPHA]))
            .save(&path)
            .expect("write the watermark");
        let mut s = settings(crate::encoders::codec::Codec::H264);
        s.watermark_path = path.to_string_lossy().into_owned();
        let Some(mut gpu) = open(&s) else {
            println!(
                "the DRI3 path declined a watermarked session on this host; nothing to capture"
            );
            return;
        };
        let grabbed = gpu.grab(false).expect("blit and composite");
        let pkt = encode_grabbed(&mut gpu, grabbed, 0, true);
        let mut dec = VideoDecoder::new(DecCodec::H264).expect("H.264 decoder");
        assert!(
            dec.decode(&pkt[VIDEO_HEADER_LEN..]).expect("decode"),
            "no picture"
        );
        let v = dec.frame().expect("decoded frame");
        // Straight-alpha source over the painted screen, which is what both paths must produce.
        let blend =
            |m: u8, bg: u8| (m as f64 * ALPHA as f64 + bg as f64 * (255.0 - ALPHA as f64)) / 255.0;
        let want = crate::encoders::chroma_siting::ycbcr(
            [
                blend(MARK.0, SCREEN.0),
                blend(MARK.1, SCREEN.1),
                blend(MARK.2, SCREEN.2),
            ],
            crate::encoders::chroma_siting::BT709,
        );
        let (mx, my) = (48usize, 48usize);
        let got = [
            v.y[my * v.y_stride + mx] as f64,
            v.u[(my / 2) * v.uv_stride + mx / 2] as f64,
            v.v[(my / 2) * v.uv_stride + mx / 2] as f64,
        ];
        for i in 0..3 {
            assert!(
                (got[i] - want[i]).abs() <= 10.0,
                "watermark plane {i}: captured {:.1}, blended {:.1}",
                got[i],
                want[i]
            );
        }
        let _ = std::fs::remove_file(&path);
        println!("the server composited the watermark at ({mx},{my}) to Y/Cb/Cr {got:?}");
    }

    /// On a DRI3 server whose screen lives on the GPU: the server imports the pool, a blit of a
    /// painted root decodes to the painted color, and a repaint reaches the next frame, which it
    /// could not if the encoder read a stale buffer. Ignored by default; run with `DISPLAY` on
    /// such a server (XLibre's Xvfb with `-glamor -dri`, an Xorg on a DRM driver) and a
    /// hardware encoder on the same device:
    /// `cargo test gpu_dri3 -- --ignored --nocapture --test-threads=1`.
    #[test]
    #[ignore]
    fn gpu_dri3_zero_copy_encodes_the_painted_root() {
        const FIRST: (u8, u8, u8) = (0x20, 0x40, 0xc0);
        const SECOND: (u8, u8, u8) = (0xd0, 0x50, 0x18);
        if !paint_root(FIRST) {
            println!("no X root this host can paint (xsetroot, $DISPLAY); nothing to capture");
            return;
        }
        let Some(mut gpu) = open(&settings(crate::encoders::codec::Codec::H264)) else {
            println!("the DRI3 path declined this session on this host; nothing to capture");
            return;
        };
        let mut dec = VideoDecoder::new(DecCodec::H264).expect("H.264 decoder");

        let encode = |gpu: &mut GpuCapture, i: u64, key: bool| -> Vec<u8> {
            let grabbed = gpu.grab(false).expect("blit into the pool");
            encode_grabbed(gpu, grabbed, i, key)
        };

        let pkt = encode(&mut gpu, 0, true);
        assert_eq!(
            parse_video_type(pkt[1]),
            Some((crate::encoders::codec::Codec::H264, FRAME_KEY))
        );
        let mean = decoded_mean(&mut dec, &pkt[VIDEO_HEADER_LEN..]);
        let want = painted_ycbcr(FIRST);
        for i in 0..3 {
            assert!(
                (mean[i] - want[i]).abs() <= 8.0,
                "plane {i}: captured {:.1}, painted {:.1}",
                mean[i],
                want[i]
            );
        }

        for i in 1..4u64 {
            let pkt = encode(&mut gpu, i, false);
            assert_eq!(
                parse_video_type(pkt[1]),
                Some((crate::encoders::codec::Codec::H264, FRAME_DELTA))
            );
            let _ = decoded_mean(&mut dec, &pkt[VIDEO_HEADER_LEN..]);
        }

        assert!(
            paint_root(SECOND),
            "the root was painted once, so a repaint must work"
        );
        let pkt = encode(&mut gpu, 4, false);
        let mean = decoded_mean(&mut dec, &pkt[VIDEO_HEADER_LEN..]);
        let want = painted_ycbcr(SECOND);
        for i in 0..3 {
            assert!(
                (mean[i] - want[i]).abs() <= 8.0,
                "after repaint, plane {i}: captured {:.1}, painted {:.1}",
                mean[i],
                want[i]
            );
        }
        println!(
            "captured {}x{} through {} server-imported buffers on {} and encoded in place on {}",
            gpu.settings.width,
            gpu.settings.height,
            POOL_N,
            gpu.x.device,
            gpu.backend()
        );
    }
}
