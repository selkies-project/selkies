/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! X11 host capture: grab the root window into host memory as BGRA, composite the XFixes hardware
//! cursor and the watermark on the CPU, and feed each frame to [`pipeline::X11Pipeline`], which owns
//! damage/stripe/encode. The grab goes through a shared-memory segment (XShm via x11rb) rather than
//! a plain `GetImage` for one reason: a full-screen frame is far too large to copy through the X
//! protocol socket every tick, so XShm has the server write the pixels straight into memory this
//! process already has mapped.
//!
//! This is the general path, and it works against any X server. Two zero-copy paths come first
//! where they can serve the session: [`nvfbc`] on an NVIDIA X server has the driver composite the
//! screen straight into video memory and registers that buffer with NVENC in place, and [`dri3`]
//! on any server whose screen lives on the GPU has the server blit the root into dmabufs the
//! hardware encoder reads in place. `run_capture` tries them in that order and falls back here
//! otherwise.
//!
//! `run_capture` splits the work across two threads because grabbing the next frame and encoding
//! the previous one have no reason to wait on each other: the caller's thread grabs frames and owns
//! the x11rb connection and the pool of shm surfaces, while a spawned encode thread owns the
//! [`pipeline::X11Pipeline`] (and thus the encoder) and runs the delivery callback, so the two overlap for
//! throughput. Frames hand off through a bounded `FramePool` that carries only a raw pointer and
//! geometry — never an X object, none of which is safe to share — so nothing X-related ever crosses
//! the thread boundary and the encoder never has to touch X. Multi-instance safety for the encoders
//! is handled inside them (e.g. the libx264 open/close lock); each capture owns its own private xcb
//! connection, so there is no shared X state to serialize here.

use std::cell::Cell;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::damage::{ConnectionExt as DamageExt, Damage, ReportLevel};
use x11rb::protocol::shm::ConnectionExt as ShmExt;
use x11rb::protocol::xfixes::{ConnectionExt as XfixesExt, Region};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt as XprotoExt, ImageFormat, PropMode, Rectangle, Window,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as WrapperExt;

use crate::pace::{FramePace, TickTrigger};

use crate::RustCaptureSettings;
use crate::encoders::overlay::blend_pixel_premultiplied;
use crate::encoders::software::{EncodedStripe, FrameTiming};
use crate::pipeline::X11Pipeline;
use crate::recording_sink::RecordingSink;

pub mod computer_use;
pub mod cursor;
pub mod dri3;
pub mod nvfbc;

/// Cross-thread controls for a running capture: a bag of atomics (plus two mutex-guarded
/// payloads) the owning `ScreenCapture` pyclass flips from the Python thread and the capture thread
/// reads at the top of each iteration.
///
/// It exists so Python never has to reach into the pipeline. The `X11Pipeline` and its encoder are
/// only safe to touch from the capture and encode threads, so `request_idr` / rate / fps / region
/// changes are posted here as atomic flags and applied later on the thread that owns the pipeline,
/// rather than mutating encoder state across the thread boundary from Python.
///
/// 1. **Lifecycle**: `stop` ends the capture loop; `force_idr` requests an on-demand keyframe on the
///    next processed frame; `reference_reports` are what clients said of frames, lost or held
///    by every one of them, which the next processed frame applies first.
/// 2. **Rate control** (gated by `rate_dirty`): `bitrate_kbps`, `vbv_mult_milli` (the VBV frame-time
///    multiplier * 1000, held as an integer for atomics; `<= 0` selects the policy default), and
///    `fps_bits` (the target fps as `f64` bits, so a fractional rate reaches the encoders exactly,
///    re-read every frame for dynamic pacing and rate control), and `link_kbps`, the rate the path
///    carries (`RustCaptureSettings::video_link_kbps`), also re-read every frame. These
///    atomics always hold the CURRENT values, so a pipeline rebuild can carry live rates forward.
/// 3. **Tunables** (gated by `tunables_dirty`): one `LiveTunables` struct behind `tunables`, set
///    rarely, carrying the per-frame quality knobs for the encode thread.
/// 4. **Capture geometry**: `capture_cursor` toggles the cursor overlay (read on every grab);
///    `region_dirty` guards `region` = `(x, y, w, h)` (w/h `<= 0` extends to the root edge), applied
///    by the capture thread with the same drain/recreate machinery as auto-adjust.
pub struct Controls {
    pub stop: AtomicBool,
    /// Set by the capture thread as it returns — after it has joined its encode thread and
    /// so dropped the NVENC/CUDA session. The atexit sweep waits on this (bounded) before
    /// letting the interpreter finalize, because that hardware-session drop racing process
    /// exit segfaults, exactly as the Wayland branch fences with a Barrier.
    pub finished: AtomicBool,
    pub force_idr: AtomicBool,
    pub reference_reports: Mutex<Vec<crate::encoders::reference::ReferenceReport>>,
    pub rate_dirty: AtomicBool,
    pub bitrate_kbps: AtomicI32,
    pub vbv_mult_milli: AtomicI32,
    pub fps_bits: AtomicU64,
    pub link_kbps: AtomicI32,
    pub tunables_dirty: AtomicBool,
    pub tunables: Mutex<Option<crate::LiveTunables>>,
    pub capture_cursor: AtomicBool,
    /// The codec the pipeline streams, as `Codec::id`, once one has been built; the
    /// selection ladder may have demoted the requested one. `u32::MAX` until then.
    pub codec: AtomicU32,
    pub region_dirty: AtomicBool,
    pub region: Mutex<(i32, i32, i32, i32)>,
    /// What this capture streams and how, for `ScreenCapture::stream_info`.
    pub report: Arc<crate::report::StreamReport>,
}

impl Controls {
    /// Seed the controls from the initial settings so the first loop iteration reads the
    /// configured bitrate / VBV / fps / region / cursor state rather than defaults.
    pub fn new(s: &RustCaptureSettings) -> Self {
        Self {
            stop: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            force_idr: AtomicBool::new(false),
            reference_reports: Mutex::new(Vec::new()),
            rate_dirty: AtomicBool::new(false),
            bitrate_kbps: AtomicI32::new(s.video_bitrate_kbps),
            vbv_mult_milli: AtomicI32::new((s.video_vbv_multiplier * 1000.0).round() as i32),
            fps_bits: AtomicU64::new(s.target_fps.max(1.0).to_bits()),
            link_kbps: AtomicI32::new(s.video_link_kbps),
            tunables_dirty: AtomicBool::new(false),
            tunables: Mutex::new(None),
            capture_cursor: AtomicBool::new(s.capture_cursor),
            codec: AtomicU32::new(u32::MAX),
            region_dirty: AtomicBool::new(false),
            region: Mutex::new((s.capture_x, s.capture_y, s.width, s.height)),
            report: crate::report::StreamReport::new("x11"),
        }
    }
}

/// A shared-memory image surface: a POSIX shm segment mapped into both this process and the
/// X server, into which `shm_get_image` writes one BGRA frame.
///
/// Both ends map the same physical pages, which is the whole point: a full-screen grab then costs no
/// per-frame copy across the X socket, because the server's blit lands directly in memory the
/// capture thread already reads. The surface owns the server-side segment id (`shmseg`), the local
/// mapping (`addr` / `size`), and the geometry (`width` / `height` / `stride`) needed to interpret
/// the bytes.
struct ShmSurface {
    shmseg: u32,
    addr: *mut u8,
    size: usize,
    width: u16,
    height: u16,
    stride: usize,
}

impl ShmSurface {
    /// Allocate a shm segment of `width*height*4` bytes, attach it both locally and to the X
    /// server, and hand back a surface both ends can read.
    ///
    /// The segment is created with `shmget(IPC_PRIVATE)` and mapped locally via `shmat`, then
    /// attached on the server (`shm_attach`). That attach is confirmed with a round-trip `check()`
    /// BEFORE the segment is marked `IPC_RMID`: deleting only after both ends hold a reference means
    /// the kernel frees the segment once this process and the server have both detached, never while
    /// either still needs it. Every failure path unwinds the partial state (local detach and/or
    /// `IPC_RMID`) so a failed allocation leaks neither memory nor a server attachment; a zero-sized
    /// request is rejected up front.
    fn create(conn: &RustConnection, width: u16, height: u16) -> Result<Self, String> {
        let stride = width as usize * 4;
        let size = stride * height as usize;
        if size == 0 {
            return Err("zero-sized capture surface".into());
        }
        unsafe {
            let shmid = libc::shmget(libc::IPC_PRIVATE, size, libc::IPC_CREAT | 0o600);
            if shmid < 0 {
                return Err("shmget failed".into());
            }
            let addr = libc::shmat(shmid, std::ptr::null(), 0);
            if addr == (-1isize) as *mut libc::c_void {
                libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut());
                return Err("shmat failed".into());
            }
            let shmseg = match conn.generate_id() {
                Ok(id) => id,
                Err(e) => {
                    libc::shmdt(addr);
                    libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut());
                    return Err(format!("generate_id: {e}"));
                }
            };
            let attach = conn
                .shm_attach(shmseg, shmid as u32, false)
                .map_err(|e| format!("shm_attach: {e}"))
                .and_then(|c| c.check().map_err(|e| format!("shm_attach check: {e}")));
            libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut());
            if let Err(e) = attach {
                libc::shmdt(addr);
                return Err(e);
            }
            Ok(Self {
                shmseg,
                addr: addr as *mut u8,
                size,
                width,
                height,
                stride,
            })
        }
    }

    /// Borrow the mapped segment as a mutable byte slice for the capture blit and overlays.
    fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.addr, self.size) }
    }

    /// Borrow the mapped segment read-only, for the stability comparison.
    fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.addr, self.size) }
    }

    /// Detach the segment from the X server and then locally, clearing the mapped pointer.
    ///
    /// This is a manual method rather than a `Drop` impl because releasing the server-side
    /// attachment is an X protocol request that needs the live connection, which a `Drop` could not
    /// reach; the owner of the connection must call it before the surface is dropped. The order —
    /// server `shm_detach` first, then the local `shmdt` — completes the release of the segment that
    /// was marked `IPC_RMID` at creation, so the kernel reclaims it once neither end holds it.
    fn destroy(&mut self, conn: &RustConnection) {
        let _ = conn.shm_detach(self.shmseg);
        let _ = conn.flush();
        unsafe {
            libc::shmdt(self.addr as *mut libc::c_void);
        }
        self.addr = std::ptr::null_mut();
    }
}

/// Reject a server whose root visual is not 32 bits per pixel.
///
/// Every `ShmSurface` is sized at `width * 4` bytes per row and the capture is read as packed
/// BGRA, so a root of any other depth would be grabbed as garbage rather than failing. Checked
/// on the initial connection and again on every reconnect, since a restarted server can come
/// back with a different visual.
fn require_32bpp(conn: &RustConnection, screen: usize) -> Result<(), String> {
    let root_depth = conn.setup().roots[screen].root_depth;
    let bpp = conn
        .setup()
        .pixmap_formats
        .iter()
        .find(|f| f.depth == root_depth)
        .map(|f| f.bits_per_pixel)
        .unwrap_or(32);
    if bpp != 32 {
        return Err(format!(
            "unsupported root depth {root_depth} ({bpp} bpp); only 32-bpp BGRA is supported"
        ));
    }
    Ok(())
}

/// Clamp a capture offset to the live root so a grab always has at least one root pixel in
/// range: a past-end offset fails every `shm_get_image` with BadMatch and would burn out the
/// failure budget with no chance of recovery. The upper clamp also keeps protocol-range roots
/// (INT16 coordinates) from truncating a wider value through the `as i16` cast.
fn clamp_offset(x: i32, root: u16) -> i16 {
    let upper = (root as i32).saturating_sub(1).min(i16::MAX as i32 - 1);
    x.max(0).min(upper) as i16
}

/// Resolve the capture dimensions `shm_get_image` will read, bounded so the region can never
/// run past the live root from the (clamped) capture offset: with auto-adjust (or unset
/// width/height `<= 0`) the capture tracks the full root minus the offset, otherwise the
/// requested size is clamped to what is available from it. Saturating subtraction plus a final
/// `u16` clamp (minimum 2) keep pathological settings from overflowing or collapsing to an
/// unusable surface. Video codecs require even dimensions, so both are then rounded down to
/// a multiple of two.
fn resolve_dims(root_w: u16, root_h: u16, s: &RustCaptureSettings) -> (u16, u16) {
    let cap_x = clamp_offset(s.capture_x, root_w) as i32;
    let cap_y = clamp_offset(s.capture_y, root_h) as i32;
    let avail_w = (root_w as i32).saturating_sub(cap_x).max(2);
    let avail_h = (root_h as i32).saturating_sub(cap_y).max(2);
    let mut w = if s.auto_adjust_screen_capture_size || s.width <= 0 {
        avail_w
    } else {
        s.width.min(avail_w)
    };
    let mut h = if s.auto_adjust_screen_capture_size || s.height <= 0 {
        avail_h
    } else {
        s.height.min(avail_h)
    };
    w = w.clamp(2, u16::MAX as i32);
    h = h.clamp(2, u16::MAX as i32);
    if s.codec.is_video() {
        w &= !1;
        h &= !1;
    }
    (w as u16, h as u16)
}

/// Rebuild the X channel + capture surfaces after a connection-level failure (Xorg restart,
/// VT switch), refreshing the offset/dims state against the live root. The pool is drained
/// first so the encode thread can never be reading a surface as it is destroyed, and its
/// generation is bumped so the encoder rebuilds against the new layout. Every fallible step
/// completes before the swap, and the old channel/surfaces are destroyed best-effort after
/// it (their server is likely gone; the local detaches still run, and the kernel frees
/// each old segment once nothing maps it).
#[allow(clippy::too_many_arguments)]
fn try_rebuild_channel(
    conn: &mut RustConnection,
    root: &mut u32,
    rsettings: &RustCaptureSettings,
    cap_x: &mut i16,
    cap_y: &mut i16,
    cap_w: &mut u16,
    cap_h: &mut u16,
    root_w: &mut u16,
    root_h: &mut u16,
    pool_n: usize,
    pool: &FramePool,
    surfaces: &mut Vec<ShmSurface>,
    stop: &AtomicBool,
) -> Result<(), String> {
    let (new_conn, new_screen) =
        x11rb::connect(None).map_err(|e| format!("X11 reconnect failed: {e}"))?;
    require_32bpp(&new_conn, new_screen)?;
    new_conn
        .shm_query_version()
        .map_err(|e| format!("shm_query_version: {e}"))?
        .reply()
        .map_err(|e| format!("XShm unavailable on reconnect: {e}"))?;
    new_conn
        .xfixes_query_version(5, 0)
        .map_err(|e| format!("xfixes_query_version: {e}"))?
        .reply()
        .map_err(|e| format!("XFixes unavailable on reconnect: {e}"))?;
    let new_root = new_conn.setup().roots[new_screen].root;
    let geo = new_conn
        .get_geometry(new_root)
        .map_err(|e| format!("get_geometry: {e}"))?
        .reply()
        .map_err(|e| format!("get_geometry reply on reconnect: {e}"))?;
    if !pool.drain_for_resize(pool_n, stop) {
        return Err("pool drain interrupted by stop during reconnect".to_string());
    }
    let (fw, fh) = resolve_dims(geo.width, geo.height, rsettings);
    let mut fresh: Vec<ShmSurface> = Vec::with_capacity(pool_n);
    for _ in 0..pool_n {
        match ShmSurface::create(&new_conn, fw, fh) {
            Ok(s) => fresh.push(s),
            Err(e) => {
                for s in fresh.iter_mut() {
                    s.destroy(&new_conn);
                }
                return Err(e);
            }
        }
    }
    for s in surfaces.iter_mut() {
        s.destroy(conn);
    }
    *conn = new_conn;
    *root = new_root;
    *cap_x = clamp_offset(rsettings.capture_x, geo.width);
    *cap_y = clamp_offset(rsettings.capture_y, geo.height);
    *cap_w = fw;
    *cap_h = fh;
    *root_w = geo.width;
    *root_h = geo.height;
    *surfaces = fresh;
    pool.bump_generation();
    Ok(())
}

/// The X server's report that the root window changed, which lets a capture publish a change as
/// it lands instead of on the next tick. `None` where the server carries no DAMAGE extension,
/// leaving the frame timer to pace the capture by itself.
///
/// Reporting is one notification per non-empty region: the server stays quiet until the region
/// is subtracted, which is what [`Self::clear`] does to re-arm the next report.
struct RootDamage {
    id: Damage,
    /// The XFixes region [`Self::take`] moves a report's damage into, to read where the root
    /// changed; None where the server's XFixes has no regions.
    parts: Option<Region>,
    /// A report read off the connection and not yet acted on. Latched here because reading the
    /// connection is what notices one, and that happens well before the frame it wakes.
    reported: Cell<bool>,
}

impl RootDamage {
    fn create(conn: &RustConnection, root: Window) -> Option<Self> {
        conn.damage_query_version(1, 1).ok()?.reply().ok()?;
        let id = conn.generate_id().ok()?;
        conn.damage_create(id, root, ReportLevel::NON_EMPTY)
            .ok()?
            .check()
            .ok()?;
        let parts = (|| {
            if conn
                .xfixes_query_version(2, 0)
                .ok()?
                .reply()
                .ok()?
                .major_version
                < 2
            {
                return None;
            }
            let parts = conn.generate_id().ok()?;
            conn.xfixes_create_region(parts, &[]).ok()?.check().ok()?;
            Some(parts)
        })();
        Some(Self {
            id,
            parts,
            reported: Cell::new(false),
        })
    }

    /// Whether the server has reported a change since the last [`Self::clear`], taking in
    /// whatever else the connection carries along the way.
    fn reported(&self, conn: &RustConnection) -> bool {
        while let Ok(Some(event)) = conn.poll_for_event() {
            if matches!(event, Event::DamageNotify(_)) {
                self.reported.set(true);
            }
        }
        self.reported.get()
    }

    /// Empty the region behind a report that has been acted on, so the next change is reported
    /// in turn; a tick nothing reported leaves the server alone. Called before a grab, which
    /// leaves a change racing the grab to wake the next frame instead of being cleared along
    /// with what the grab captured.
    fn clear(&self, conn: &RustConnection) {
        if !self.reported(conn) {
            return;
        }
        self.reported.set(false);
        let _ = conn.damage_subtract(self.id, x11rb::NONE, x11rb::NONE);
        let _ = conn.flush();
    }

    /// [`Self::clear`], saying where the root changed: the damaged rectangles, empty where
    /// nothing was reported, None where a change was reported but the server cannot say where.
    fn take(&self, conn: &RustConnection) -> Option<Vec<Rectangle>> {
        if !self.reported(conn) {
            return Some(Vec::new());
        }
        let Some(parts) = self.parts else {
            self.clear(conn);
            return None;
        };
        self.reported.set(false);
        conn.damage_subtract(self.id, x11rb::NONE, parts).ok()?;
        Some(
            conn.xfixes_fetch_region(parts)
                .ok()?
                .reply()
                .ok()?
                .rectangles,
        )
    }
}

/// [`RootDamage::take`] for a tick `trigger` woke: whether the root changed since the last
/// frame, and where. A report the take finds counts whatever woke the tick: one landing after
/// the wait gave up is spent here all the same, and called clean, its change would be cleared
/// with nothing left to report it (a capture that sends only changes would never send it).
fn take_tick(
    damage: &RootDamage,
    conn: &RustConnection,
    trigger: TickTrigger,
) -> (bool, Option<Vec<Rectangle>>) {
    let rects = damage.take(conn);
    let dirty =
        trigger == TickTrigger::Damage || rects.as_ref().is_none_or(|rects| !rects.is_empty());
    (dirty, rects)
}

/// The share of the captured area at (`x`, `y`), `width` x `height`, that the root's damaged
/// `rects` cover.
fn captured_damage(
    rects: &[Rectangle],
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> crate::pipeline::Damage {
    let local: Vec<smithay::utils::Rectangle<i32, smithay::utils::Physical>> = rects
        .iter()
        .map(|r| {
            smithay::utils::Rectangle::new(
                (i32::from(r.x) - x, i32::from(r.y) - y).into(),
                (i32::from(r.width), i32::from(r.height)).into(),
            )
        })
        .collect();
    crate::pipeline::Damage::of_rects(&local, width, height)
}

/// Wait for the connection to carry something, for at most `timeout`.
fn readable(conn: &RustConnection, timeout: Duration) -> bool {
    let mut poll = libc::pollfd {
        fd: conn.stream().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let deadline = libc::timespec {
        tv_sec: timeout.as_secs() as _,
        tv_nsec: timeout.subsec_nanos() as libc::c_long,
    };
    unsafe { libc::ppoll(&mut poll, 1, &deadline, std::ptr::null()) > 0 }
}

/// Block until the capture is due a frame and say what asked for it: a change the server
/// reported, brought forward by as much as the pacing budget allows, or the frame timer.
///
/// Without DAMAGE the wait is the timer's alone, which is the cadence a capture keeps when
/// nothing else can say the screen moved.
fn wait_for_frame(
    conn: &RustConnection,
    damage: Option<&RootDamage>,
    pace: &FramePace,
    period: Duration,
) -> TickTrigger {
    let now = Instant::now();
    let due = pace.next_due(period, now);
    let sleep_until = |t: Instant| {
        if let Some(wait) = t.checked_duration_since(Instant::now()) {
            thread::sleep(wait);
        }
    };
    let Some(damage) = damage else {
        sleep_until(due);
        return TickTrigger::Timer;
    };
    // Nothing may be published before the budget allows it, so the report of a change that
    // lands earlier is left queued until then rather than read and cleared.
    sleep_until(pace.pull_at(TickTrigger::Damage, period, now).min(due));
    loop {
        if damage.reported(conn) {
            return TickTrigger::Damage;
        }
        let Some(wait) = due.checked_duration_since(Instant::now()) else {
            return TickTrigger::Timer;
        };
        if !readable(conn, wait) {
            return TickTrigger::Timer;
        }
    }
}

/// One `shm_get_image` request is atomic — the server never interleaves another client
/// inside it — so the grab is a coherent snapshot of whatever the screen held at that
/// instant. Every grabbed frame is published; whether any of it changed is decided
/// downstream by the encoder's own per-stripe damage detection.
fn grab_frame(
    conn: &RustConnection,
    root: u32,
    surface: &ShmSurface,
    cap_x: i16,
    cap_y: i16,
    cap_w: u16,
    cap_h: u16,
) -> Result<(), String> {
    conn.shm_get_image(
        root,
        cap_x,
        cap_y,
        cap_w,
        cap_h,
        !0u32,
        ImageFormat::Z_PIXMAP.into(),
        surface.shmseg,
        0,
    )
    .map_err(|e| format!("shm_get_image: {e}"))?
    .reply()
    .map_err(|e| format!("shm_get_image reply: {e}"))?;
    Ok(())
}

/// Frame-space top-left of the cursor image, given the XFixes hotspot position.
///
/// XFixes reports the cursor position at its HOTSPOT; X draws the image with its top-left at
/// `(pos - hot)`, and the capture origin `(cap_x, cap_y)` is subtracted to move it into frame space.
/// The result may go negative near the frame edges, which `overlay_cursor` clips per pixel to match
/// the server's own edge clipping.
#[inline]
pub(crate) fn cursor_image_origin(
    x: i16,
    y: i16,
    xhot: u16,
    yhot: u16,
    cap_x: i32,
    cap_y: i32,
) -> (i32, i32) {
    (
        x as i32 - xhot as i32 - cap_x,
        y as i32 - yhot as i32 - cap_y,
    )
}

/// Composite the XFixes cursor (ARGB `u32` per pixel, premultiplied by the XFixes
/// format definition) onto the BGRA frame with its top-left at `(img_x, img_y)`,
/// blending each pixel through `blend_pixel_premultiplied` with per-pixel bounds
/// clipping so an image straddling a frame edge writes only its in-frame portion.
#[allow(clippy::too_many_arguments)]
pub(crate) fn overlay_cursor(
    frame: &mut [u8],
    stride: usize,
    frame_w: i32,
    frame_h: i32,
    cur_w: i32,
    cur_h: i32,
    pixels: &[u32],
    img_x: i32,
    img_y: i32,
) {
    for y in 0..cur_h {
        let ty = img_y + y;
        if ty < 0 || ty >= frame_h {
            continue;
        }
        for x in 0..cur_w {
            let tx = img_x + x;
            if tx < 0 || tx >= frame_w {
                continue;
            }
            let px = pixels[(y * cur_w + x) as usize];
            let a = ((px >> 24) & 0xFF) as u8;
            let r = ((px >> 16) & 0xFF) as u8;
            let g = ((px >> 8) & 0xFF) as u8;
            let b = (px & 0xFF) as u8;
            let off = ty as usize * stride + tx as usize * 4;
            blend_pixel_premultiplied(&mut frame[off..off + 4], r, g, b, a);
        }
    }
}

/// A captured raw BGRA frame held in a pooled shm surface, ready to encode.
///
/// It carries the surface pointer plus geometry so the encode thread reads the pixels directly (no
/// copy) and can rebuild its pipeline when the capture size changed (auto-adjust). It also carries
/// the pool's surface `generation`, so a rebuild is still triggered when the surfaces were recreated
/// at the SAME size (a resize flap) and the new segments happen to reuse the old virtual addresses.
struct RawFrame {
    idx: usize,
    ptr: *mut u8,
    len: usize,
    width: u16,
    height: u16,
    stride: usize,
    generation: u64,
    /// CLOCK_MONOTONIC nanoseconds at which the pixels were in hand.
    captured_ns: i64,
}
/// `RawFrame` is `Send`: its raw pointer addresses a pooled shm surface that the pool
/// guarantees is not reused until the encode thread recycles this frame, so the handle is safe to
/// move across the capture -> encode thread boundary.
unsafe impl Send for RawFrame {}

/// Mutex-guarded interior of `FramePool`: the free-surface index list and the single
/// capture -> encode handoff slot, under one lock so moving a surface between them — acquire,
/// publish, take, recycle — is always a single atomic step.
struct PoolInner {
    free: Vec<usize>,
    slot: Option<RawFrame>,
}

/// Demand-driven capture -> encode handoff: a bounded single-slot channel over a fixed set of
/// pooled shm surfaces.
///
/// The capture thread writes into a pooled surface and `publish`es it into the single `slot`; the
/// encode thread `take`s it. Capture stays at most one frame ahead of encode because `acquire` and
/// `publish` BLOCK (bounded) until the encoder frees a surface / drains the slot, throttling capture
/// to the encode rate. Since X11 capture is pull-based (no backlog), throttling — rather than
/// capturing-then-dropping — means a full-resolution shm round-trip is never spent on a frame that
/// would be discarded, while the next capture still overlaps the current encode (the throughput win).
/// The encoder therefore only ever sees a contiguous frame stream, keeping the H.264 reference chain
/// valid.
///
/// `generation` is bumped by the capture thread each time the backing shm surfaces are destroyed and
/// recreated. Published frames carry it so the encode thread rebuilds its pipeline — dropping encoder
/// state keyed to surface base pointers (e.g. NVENC's pinned-host registrations) — even when a resize
/// flap lands back on the old dimensions and the recreated segments reuse the old virtual addresses.
struct FramePool {
    inner: Mutex<PoolInner>,
    cv: Condvar,
    stop: AtomicBool,
    generation: AtomicU64,
}

impl FramePool {
    /// Create a pool with `n` free surfaces, an empty handoff slot, and generation 0.
    fn new(n: usize) -> Self {
        Self {
            inner: Mutex::new(PoolInner {
                free: (0..n).collect(),
                slot: None,
            }),
            cv: Condvar::new(),
            stop: AtomicBool::new(false),
            generation: AtomicU64::new(0),
        }
    }

    /// Capture: record that the surfaces were recreated by advancing the generation. Only
    /// called after `drain_for_resize` succeeded, so no frame from the previous generation is still
    /// in flight.
    fn bump_generation(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// Read the current surface generation, stamped onto each published frame.
    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Capture: claim a free surface to write the next frame into, or `None` on stop.
    ///
    /// Blocks (bounded, re-checking `stop` every 20ms) until a surface is free, which throttles
    /// capture to the encode rate so a full-resolution capture is never spent on a frame that would
    /// be dropped. The bounded re-check means a stop that races the wait cannot hang the thread.
    fn acquire(&self, stop: &AtomicBool) -> Option<usize> {
        let mut g = self.inner.lock().unwrap();
        loop {
            if let Some(idx) = g.free.pop() {
                return Some(idx);
            }
            if stop.load(Ordering::Relaxed) {
                return None;
            }
            let (gg, _) = self.cv.wait_timeout(g, Duration::from_millis(20)).unwrap();
            g = gg;
        }
    }

    /// Capture: publish the just-captured frame into the single slot, or `false` (frame
    /// discarded) on stop.
    ///
    /// Blocks (bounded, re-checking `stop` every 20ms) until the encode thread has taken the previous
    /// frame, so capture stays at most one frame ahead and never drops under normal flow.
    fn publish(&self, frame: RawFrame, stop: &AtomicBool) -> bool {
        let mut g = self.inner.lock().unwrap();
        loop {
            if g.slot.is_none() {
                g.slot = Some(frame);
                drop(g);
                self.cv.notify_all();
                return true;
            }
            if stop.load(Ordering::Relaxed) {
                return false;
            }
            let (gg, _) = self.cv.wait_timeout(g, Duration::from_millis(20)).unwrap();
            g = gg;
        }
    }

    /// Encode: block until a frame is available (`Some`) or stop is signaled (`None`).
    ///
    /// The wait is bounded (re-checking `stop` every 20ms) as defense-in-depth against a lost wakeup,
    /// so a stop that races the park can never leave the encode thread blocked forever.
    fn take(&self) -> Option<RawFrame> {
        let mut g = self.inner.lock().unwrap();
        loop {
            if let Some(f) = g.slot.take() {
                return Some(f);
            }
            if self.stop.load(Ordering::Acquire) {
                return None;
            }
            let (gg, _) = self.cv.wait_timeout(g, Duration::from_millis(20)).unwrap();
            g = gg;
        }
    }

    /// Encode: return a surface to the free list after it has been encoded, waking any waiter
    /// parked in `acquire` / `publish` / `drain_for_resize`.
    fn recycle(&self, idx: usize) {
        self.inner.lock().unwrap().free.push(idx);
        self.cv.notify_all();
    }

    /// Capture: before recreating surfaces (auto-adjust / region resize), reclaim the pending
    /// slot and wait until every surface is back in the free list, so no surface is destroyed while
    /// the encode thread is still reading it.
    ///
    /// Reclaiming the slot returns any un-taken frame to the free list; the loop then waits until
    /// `free.len() == n`, i.e. the encode thread has finished any in-flight frame. The wait is bounded
    /// and also breaks on stop — pool shutdown OR the external `stop` — so a panicked or dead encode
    /// thread that never recycles cannot wedge the capture thread here forever; a requested stop
    /// unblocks it. Returns `true` if it fully drained (safe to recreate surfaces), or `false` if it
    /// aborted on stop — in which case the caller tears down, which joins the encode thread before
    /// destroying surfaces, so the resize-safety guarantee still holds.
    fn drain_for_resize(&self, n: usize, stop: &AtomicBool) -> bool {
        let mut g = self.inner.lock().unwrap();
        if let Some(old) = g.slot.take() {
            g.free.push(old.idx);
        }
        while g.free.len() < n {
            if self.stop.load(Ordering::Acquire) || stop.load(Ordering::Relaxed) {
                return false;
            }
            let (gg, _) = self.cv.wait_timeout(g, Duration::from_millis(20)).unwrap();
            g = gg;
        }
        true
    }

    /// Signal every waiter to stop and wake them, ending the encode thread's `take` loop.
    ///
    /// The inner mutex is acquired BEFORE storing `stop` and notifying, which closes the lost-wakeup
    /// window: with the lock held, `take` is either still before its `stop` check or already parked on
    /// the condvar (and will receive the notify), never in the gap between the two. The notify is
    /// issued after the guard is dropped so the woken thread does not immediately re-block on the lock
    /// this call holds.
    fn shutdown(&self) {
        let g = self.inner.lock().unwrap();
        self.stop.store(true, Ordering::Release);
        drop(g);
        self.cv.notify_all();
    }
}

/// Encode thread body: consume captured frames from the pool, keep the [`pipeline::X11Pipeline`] in
/// step with the capture size and cross-thread controls, encode, recycle, and deliver.
///
/// The loop runs until `FramePool::take` returns `None` (stop). For each frame:
///
/// 1. **(Re)build the pipeline** when it is missing, the frame size changed, or the surface
///    generation changed. A generation change alone forces the rebuild path because recreated shm
///    segments often reuse the old virtual base addresses, so encoder state keyed to base pointers
///    (NVENC's pinned-host cache) must be dropped even at identical dimensions. An in-place
///    [`X11Pipeline::reshape`] is preferred — NVENC reconfigures its live session and the striped
///    software path just re-derives stripe state — so a resize never stalls on a full encoder
///    re-init; encoders that cannot follow in place (VAAPI) report `false` and the pipeline
///    is rebuilt. On a rebuild the old pipeline (with its GPU session/surfaces) is dropped BEFORE the
///    new one is built, so an auto-adjust resize never holds two full encoder allocations at once
///    (transient 2x GPU memory). The rebuilt settings pull the CURRENT bitrate / VBV / fps from the
///    controls atomics, carrying live rate changes forward instead of reverting to the capture-start
///    values.
/// 2. **Apply cross-thread controls** here, on the thread that owns the pipeline: a pending
///    `force_idr` requests a keyframe; a `rate_dirty` swap (Acquire, pairing with the setters' Release
///    stores so the bitrate / VBV / fps payload is never seen half-applied) pushes a live rate change;
///    a `tunables_dirty` swap applies per-frame tunables into both the local settings copy and the
///    pipeline.
/// 3. **Encode and hand off**: read the pooled surface directly through its raw pointer — sound
///    because the pool guarantees the surface is not reused until it is recycled — run
///    [`X11Pipeline::process`], recycle the surface BEFORE delivering so a slow consumer never holds a
///    capture surface, then deliver any encoded stripes through `on_frame`.
///
/// The optional Unix-socket recording sink (parity with the Wayland path) is bound ONCE here and
/// owned outside the pipeline, so pipeline rebuilds on resize keep the socket listener and any
/// attached recorders alive. It can only carry a single full-frame H.264 stream, so configurations
/// that cannot produce one (JPEG output, or a striped CPU encoder) are warned about up front.
fn encode_loop<F>(
    pool: &FramePool,
    controls: &Controls,
    settings: &RustCaptureSettings,
    on_frame: &mut F,
) where
    F: FnMut(Vec<EncodedStripe>),
{
    let _report = crate::report::enter(&controls.report);
    let mut psettings = settings.clone();
    let recording_sink = RecordingSink::try_bind(&settings.recording_socket, settings.target_fps);
    if recording_sink.is_some() {
        if !settings.codec.is_video() {
            eprintln!(
                "[recording_sink] recording_socket set but the codec is JPEG; no recordable video stream."
            );
        } else if settings.use_cpu && !settings.video_fullframe {
            eprintln!(
                "[recording_sink] recording_socket set but the CPU encoder is striped; set video_fullframe=true for a recordable stream."
            );
        }
    }
    let mut pipeline: Option<X11Pipeline> = None;
    let (mut pw, mut ph) = (0i32, 0i32);
    let mut pgen = 0u64;
    let mut last_log_time = Instant::now();
    let mut frame_count: u64 = 0;
    let mut stripe_count: u64 = 0;

    while let Some(frame) = pool.take() {
        let (fw, fh) = (frame.width as i32, frame.height as i32);
        if pipeline.is_none() || pw != fw || ph != fh || pgen != frame.generation {
            let size_changed = pw != fw || ph != fh;
            psettings.width = fw;
            psettings.height = fh;
            psettings.video_bitrate_kbps = controls.bitrate_kbps.load(Ordering::Relaxed);
            psettings.video_vbv_multiplier =
                controls.vbv_mult_milli.load(Ordering::Relaxed) as f64 / 1000.0;
            psettings.target_fps =
                f64::from_bits(controls.fps_bits.load(Ordering::Relaxed)).max(1.0);
            let reshaped = pipeline
                .as_mut()
                .is_some_and(|pl| pl.reshape(&psettings, size_changed));
            if !reshaped {
                drop(pipeline.take());
                pipeline = Some(X11Pipeline::new(psettings.clone()));
                if let Some(pl) = &pipeline {
                    let mut log_msg = format!(
                        "[X11] Stream settings active -> Res: {}x{} | FPS: {:.1} | Encoder: {}",
                        psettings.width,
                        psettings.height,
                        psettings.target_fps,
                        pl.encoder_name()
                    );
                    let fixed = pl.fixed_rate_control();
                    let paint_over =
                        crate::encoders::paint_over_desc(&psettings, fixed, pl.holds_quantizer());
                    if !pl.codec().is_video() {
                        log_msg.push_str(&format!(
                            " | Mode: JPEG | Quality: {}",
                            psettings.jpeg_quality
                        ));
                        if let Some(paint_over) = paint_over {
                            log_msg.push_str(&format!(" | {paint_over}"));
                        }
                    } else {
                        log_msg.push_str(&format!(
                            " | Mode: {} | {}",
                            pl.codec().display(),
                            crate::encoders::rate_desc(&psettings, fixed)
                        ));
                        if let Some(paint_over) = paint_over {
                            log_msg.push_str(&format!(" | {paint_over}"));
                        }
                        log_msg.push_str(&format!(" | Colorspace: {}", pl.colorspace_desc()));
                    }
                    log_msg.push_str(&format!(
                        " | Damage Thresh: {}f | Damage Dur: {}f",
                        psettings.damage_block_threshold, psettings.damage_block_duration
                    ));
                    println!("{}", log_msg);
                }
            }
            pw = fw;
            ph = fh;
            pgen = frame.generation;
        }
        let pl = pipeline.as_mut().unwrap();

        // A recorder connecting to the socket sink needs a fresh decode entry point; it is
        // folded into the same standard request-IDR path as a client-driven force_idr.
        let sink_idr = recording_sink
            .as_ref()
            .map(|s| s.should_force_idr())
            .unwrap_or(false);
        for report in std::mem::take(&mut *controls.reference_reports.lock().unwrap()) {
            pl.take_report(report);
        }
        if controls.force_idr.swap(false, Ordering::Relaxed) || sink_idr {
            pl.request_idr();
        }
        if controls.rate_dirty.swap(false, Ordering::Acquire) {
            let b = controls.bitrate_kbps.load(Ordering::Relaxed);
            let v = controls.vbv_mult_milli.load(Ordering::Relaxed) as f64 / 1000.0;
            let fps = f64::from_bits(controls.fps_bits.load(Ordering::Relaxed)).max(1.0);
            pl.update_rate(b, v, fps);
        }
        if controls.tunables_dirty.swap(false, Ordering::Acquire)
            && let Some(t) = controls.tunables.lock().unwrap().take()
        {
            t.apply_to(&mut psettings);
            pl.update_tunables(&t);
        }

        let buf = unsafe { std::slice::from_raw_parts(frame.ptr, frame.len) };
        let encode_start_ns = crate::wayland::host::now_ns();
        pl.set_link_rate(controls.link_kbps.load(Ordering::Relaxed));
        let mut stripes = pl.process(buf, frame.stride);
        FrameTiming::stamp(&mut stripes, frame.captured_ns, encode_start_ns);
        controls.codec.store(pl.codec().id(), Ordering::Relaxed);
        pool.recycle(frame.idx);
        if !stripes.is_empty() {
            frame_count += 1;
            stripe_count += stripes.len() as u64;
            // IDR arming is consumed inside X11Pipeline::process; this tap only fans out.
            if let Some(ref socket) = recording_sink {
                socket.write_frame(&stripes, psettings.width, psettings.height);
            }
            on_frame(stripes);
        }

        let now = Instant::now();
        let elapsed = now.duration_since(last_log_time).as_secs_f64();
        if elapsed >= 1.0 {
            crate::log::debug!(
                "[X11] Res: {}x{} Encoder: {} EncFPS: {:.2} EncStripes/s: {:.2}",
                psettings.width,
                psettings.height,
                pl.encoder_name(),
                frame_count as f64 / elapsed,
                stripe_count as f64 / elapsed
            );
            frame_count = 0;
            stripe_count = 0;
            last_log_time = now;
        }
    }
}

/// The rate the X server's fake vblank runs at while this process captures it: the fastest live
/// capture's frame rate, published on the root window as `_FAKE_SCREEN_MILLIHZ` (CARDINAL,
/// millihertz, 1000 to 1000000) and, rounded to whole frames for servers that read only that,
/// `_FAKE_SCREEN_FPS` (1 to 1000), both deleted when the last capture ends. The Selkies Xvfb paces
/// the clients that wait on Present by it, keeping the rate it started at as a floor unless the
/// published one is at most 0.1% slower (59.94 under 60), so a vsynced application presents as
/// fast as it is streamed; any other server holds it as inert data.
struct VblankRate {
    captures: Vec<Arc<Controls>>,
    /// Millihertz last published, 0 for none.
    published: u32,
    /// The connection, the root window, and the millihertz and frames-per-second atoms.
    conn: Option<(RustConnection, Window, Atom, Atom)>,
}

static VBLANK_RATE: Mutex<VblankRate> = Mutex::new(VblankRate {
    captures: Vec::new(),
    published: 0,
    conn: None,
});

impl VblankRate {
    fn publish(&mut self) {
        let want = self
            .captures
            .iter()
            .map(|c| {
                (f64::from_bits(c.fps_bits.load(Ordering::Relaxed)) * 1000.0)
                    .round()
                    .clamp(1000.0, 1_000_000.0) as u32
            })
            .max()
            .unwrap_or(0);
        if want == self.published {
            return;
        }
        if self.conn.is_none() {
            self.conn = x11rb::connect(None).ok().and_then(|(conn, screen)| {
                let root = conn.setup().roots[screen].root;
                let atom =
                    |name: &[u8]| Some(conn.intern_atom(false, name).ok()?.reply().ok()?.atom);
                let millihz = atom(b"_FAKE_SCREEN_MILLIHZ")?;
                let fps = atom(b"_FAKE_SCREEN_FPS")?;
                Some((conn, root, millihz, fps))
            });
        }
        let sent = self
            .conn
            .as_ref()
            .is_some_and(|(conn, root, millihz, fps)| {
                let queued = if want > 0 {
                    [(*millihz, want), (*fps, (want + 500) / 1000)]
                        .iter()
                        .all(|&(atom, value)| {
                            conn.change_property32(
                                PropMode::REPLACE,
                                *root,
                                atom,
                                AtomEnum::CARDINAL,
                                &[value],
                            )
                            .is_ok()
                        })
                } else {
                    [*millihz, *fps]
                        .iter()
                        .all(|&atom| conn.delete_property(*root, atom).is_ok())
                };
                // A round trip, not a flush: a server may drop what is still queued when a client
                // closes, and the connection closes right after the last capture's delete.
                queued
                    && conn
                        .get_input_focus()
                        .ok()
                        .and_then(|c| c.reply().ok())
                        .is_some()
            });
        self.published = if sent { want } else { 0 };
        if !sent || want == 0 {
            self.conn = None;
        }
    }
}

/// A running capture's place in [`VblankRate`], given up when the capture ends.
struct VblankClaim(Arc<Controls>);

impl VblankClaim {
    fn new(controls: &Arc<Controls>) -> Self {
        let mut rate = VBLANK_RATE.lock().unwrap();
        rate.captures.push(controls.clone());
        rate.publish();
        Self(controls.clone())
    }
}

impl Drop for VblankClaim {
    fn drop(&mut self) {
        let mut rate = VBLANK_RATE.lock().unwrap();
        rate.captures.retain(|c| !Arc::ptr_eq(c, &self.0));
        rate.publish();
    }
}

/// Republish [`VblankRate`] after a running capture's frame rate changed.
pub fn follow_frame_rate() {
    VBLANK_RATE.lock().unwrap().publish();
}

/// Capture an X11 display until `stop` is set, on the best path this session can run.
///
/// [`nvfbc::run_capture`] is tried first: where the NVIDIA driver offers framebuffer capture and
/// the session encodes on NVENC, the screen is composited into video memory and encoded in place,
/// so a frame reaches the bitstream without being copied once. [`dri3::run_capture`] is next: on a
/// server whose screen lives on the GPU, the server blits the root into dmabufs this process
/// allocated and the hardware session reads them in place. Each reports that it cannot serve the
/// session — a codec its engine has no support for, software encoding, a watermark that has to be
/// blended into host pixels, a server or device that does not qualify — and the capture then runs
/// on the general XShm path below, which every X server supports. Whichever path runs, the
/// capture's frame rate paces the server's fake vblank for as long as it does ([`VblankRate`]).
///
/// Blocking; intended to run on a dedicated thread.
pub fn run_capture<F>(
    settings: RustCaptureSettings,
    controls: Arc<Controls>,
    encode_tid_tx: Sender<thread::ThreadId>,
    mut on_frame: F,
) -> Result<(), String>
where
    F: FnMut(Vec<EncodedStripe>) + Send + 'static,
{
    let _report = crate::report::enter(&controls.report);
    let _vblank = VblankClaim::new(&controls);
    if let Some(result) = nvfbc::run_capture(
        settings.clone(),
        controls.clone(),
        encode_tid_tx.clone(),
        &mut on_frame,
    ) {
        return result;
    }
    if let Some(result) = dri3::run_capture(
        settings.clone(),
        controls.clone(),
        encode_tid_tx.clone(),
        &mut on_frame,
    ) {
        return result;
    }
    crate::report::capture("XShm", false);
    run_shm_capture(settings, controls, encode_tid_tx, on_frame)
}

/// Run the X11 capture pipeline until `stop` is set, splitting capture and encode across two
/// threads that overlap for throughput.
///
/// This (the caller's) thread performs setup and then the capture loop; a spawned encode thread runs
/// `encode_loop` and invokes `on_frame(stripes)` once per encoded frame. The split lets capture and
/// encode overlap, and because frames are throttled/dropped as RAW frames before encode, the delivered
/// H.264 stays a valid contiguous reference chain.
///
/// 1. **Setup**: connect to X (a private connection) and require a 32-bpp BGRA root — the Z-pixmap
///    byte depth must be 4, which modern servers use for depth 24/32 — then negotiate XShm and
///    XFixes. XFixes is always negotiated (one round-trip) so the cursor overlay can be toggled on
///    live even when capture started without it. Initial dimensions and origin are resolved from the
///    settings and the live root geometry, and a `FramePool` of `POOL_N` = 3 shm surfaces is
///    allocated — the working set (one in-capture, one in-slot, one in-encode) that a one-frame-ahead
///    demand-driven capture needs, which also keeps the memory cost (`3 * W*H*4`, significant at 4K)
///    bounded.
/// 2. **Encode thread**: spawned with a raised scheduling priority; it reports its thread id back
///    through `encode_tid_tx` so the caller can detect a re-entrant stop issued from inside the
///    delivery callback.
/// 3. **Capture loop** (until `stop`): fps is re-read each iteration for live pacing. The wait
///    runs to the frame deadline, but a region the X server reports damaged ends it early once
///    the shared pacing budget allows (`crate::pace`), so a screen that changes mid-period is
///    published then rather than on the next tick. A server without DAMAGE keeps the deadline
///    alone.
///    - **Live region change** (`region_dirty`): re-target the grab origin immediately (an x/y pan
///      needs no surface work); a size change reuses the drain/recreate path below.
///    - **Auto-adjust**: on a root geometry change, drain in-flight frames then recreate the surfaces
///      and bump the generation (which the encode thread turns into a reshape or rebuild). The geometry
///      is re-resolved AFTER the drain, because a slow drain can outlast another geometry change (a
///      fast flap) and recreating at a stale size would make the next `shm_get_image` exceed the root;
///      a fully-reverted flap keeps its surfaces as-is. A stop that races the drain breaks out to
///      teardown.
///    - **Grab**: acquire a pooled surface (blocks until the encoder frees one), then `shm_get_image`
///      the region into it synchronously (`reply()` waits).
///    - **Overlays**: composite the XFixes cursor (drawn at its hotspot-offset origin; live-toggleable)
///      and the watermark onto the BGRA pixels on the CPU.
///    - **Publish**: hand the finished frame to the encode thread (blocks until the slot is free;
///      never drops). A stop observed while waiting in acquire/publish exits the loop.
/// 4. **Teardown**: stop and JOIN the encode thread BEFORE destroying the shm surfaces it may still be
///    reading, preserving the resize-safety guarantee.
///
/// Blocking; intended to run on a dedicated thread. The X connection and shm surfaces live on this
/// thread and the encoder lives on the encode thread — nothing X-related crosses the boundary.
fn run_shm_capture<F>(
    settings: RustCaptureSettings,
    controls: Arc<Controls>,
    encode_tid_tx: Sender<thread::ThreadId>,
    on_frame: F,
) -> Result<(), String>
where
    F: FnMut(Vec<EncodedStripe>) + Send + 'static,
{
    let (mut conn, screen_num) =
        x11rb::connect(None).map_err(|e| format!("X11 connect failed: {e}"))?;
    let mut root = conn.setup().roots[screen_num].root;
    require_32bpp(&conn, screen_num)?;

    conn.shm_query_version()
        .map_err(|e| format!("shm_query_version: {e}"))?
        .reply()
        .map_err(|e| format!("XShm unavailable: {e}"))?;
    conn.xfixes_query_version(5, 0)
        .map_err(|e| format!("xfixes_query_version: {e}"))?
        .reply()
        .map_err(|e| format!("XFixes unavailable: {e}"))?;
    let mut damage = RootDamage::create(&conn, root);

    let geo = conn
        .get_geometry(root)
        .map_err(|e| format!("get_geometry: {e}"))?
        .reply()
        .map_err(|e| format!("get_geometry reply: {e}"))?;

    let mut rsettings = settings.clone();
    let (mut cap_w, mut cap_h) = resolve_dims(geo.width, geo.height, &rsettings);
    let mut cap_x = clamp_offset(rsettings.capture_x, geo.width);
    let mut cap_y = clamp_offset(rsettings.capture_y, geo.height);
    // Last root size seen. A live pan has to clamp against something even when the geometry
    // round-trip fails, or the requested origin would be dropped instead of applied.
    let (mut root_w, mut root_h) = (geo.width, geo.height);

    const POOL_N: usize = 3;
    let mut surfaces: Vec<ShmSurface> = Vec::with_capacity(POOL_N);
    for _ in 0..POOL_N {
        surfaces.push(ShmSurface::create(&conn, cap_w, cap_h)?);
    }
    // Consecutive channel rebuilds without one good grab in between: geometry
    // errors can burn out the grab budget repeatedly under a dead region, so
    // recovery only gets a bounded number of channel rebuilds.
    let mut reconnect_streak = 0u32;
    let pool = Arc::new(FramePool::new(POOL_N));

    let mut watermark = crate::encoders::overlay::OverlayState::default();
    if !settings.watermark_path.is_empty() {
        watermark.load_watermark(&settings.watermark_path, 1.0);
    }

    let enc_pool = pool.clone();
    let enc_controls = controls.clone();
    let enc_settings = settings.clone();
    let encode_panicked = Arc::new(AtomicBool::new(false));
    let guard_panicked = encode_panicked.clone();
    let encode_thread = thread::spawn(move || {
        crate::boost_thread_priority(-10);
        let _ = encode_tid_tx.send(thread::current().id());
        let mut on_frame = on_frame;
        // An encoder panic must never wedge the capture: without the pool shutdown
        // the capture thread would block in publish forever while is_capturing still
        // reports true on this thread's handle. Shutting the pool unblocks it through
        // the same path a clean stop takes, so the panic is also recorded — otherwise
        // the capture would return Ok and Python could not tell the two apart.
        let guard_pool = enc_pool.clone();
        let guard_controls = enc_controls.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            encode_loop(&enc_pool, &enc_controls, &enc_settings, &mut on_frame);
        }));
        if result.is_err() {
            eprintln!("[X11] encode thread panicked; shutting the pool to fail the capture");
            guard_panicked.store(true, Ordering::Release);
            guard_pool.shutdown();
            guard_controls.stop.store(true, Ordering::Relaxed);
        }
    });

    // One frame a period at most, a change taking its period's frame as it lands (FramePace::windowed).
    let mut pace = FramePace::windowed();
    // Frames between root-geometry polls (auto-adjust); ~0.5s at 60fps.
    const GEOMETRY_POLL_FRAMES: i32 = 30;
    let mut geometry_check = 0i32;
    let mut grab_failures = 0u32;

    let result = (|| -> Result<(), String> {
        while !controls.stop.load(Ordering::Relaxed) {
            let fps = f64::from_bits(controls.fps_bits.load(Ordering::Relaxed)).max(1.0);
            let frame_dur = Duration::from_secs_f64(1.0 / fps.max(1.0));
            if pace.next_due(frame_dur, Instant::now()) <= Instant::now() {
                thread::yield_now();
            }
            let trigger = wait_for_frame(&conn, damage.as_ref(), &pace, frame_dur);
            let woke = Instant::now();
            // Every grab is published; without Damage the still check never matters, since
            // nothing pulls a frame forward.
            pace.record_tick(trigger, frame_dur, woke, true);
            if controls.stop.load(Ordering::Relaxed) {
                break;
            }

            if controls.region_dirty.swap(false, Ordering::Acquire) {
                let (nx, ny, nw, nh) = *controls.region.lock().unwrap();
                rsettings.capture_x = nx;
                rsettings.capture_y = ny;
                rsettings.width = nw;
                rsettings.height = nh;
                // An explicit live size pins the region: auto-adjust (from the start
                // settings) would glue resolve_dims back to the ROOT size, undoing the
                // re-target on the next geometry poll. w/h <= 0 keep following the root.
                rsettings.auto_adjust_screen_capture_size = nw <= 0 || nh <= 0;
                cap_x = clamp_offset(nx, root_w);
                cap_y = clamp_offset(ny, root_h);
                if let Some(g) = conn.get_geometry(root).ok().and_then(|c| c.reply().ok()) {
                    root_w = g.width;
                    root_h = g.height;
                    cap_x = clamp_offset(nx, root_w);
                    cap_y = clamp_offset(ny, root_h);
                    let (fw, fh) = resolve_dims(g.width, g.height, &rsettings);
                    if fw != cap_w || fh != cap_h {
                        if !pool.drain_for_resize(POOL_N, &controls.stop) {
                            break;
                        }
                        for s in surfaces.iter_mut() {
                            s.destroy(&conn);
                        }
                        surfaces.clear();
                        cap_w = fw;
                        cap_h = fh;
                        for _ in 0..POOL_N {
                            surfaces.push(ShmSurface::create(&conn, cap_w, cap_h)?);
                        }
                        pool.bump_generation();
                    }
                }
            }

            // Root geometry is polled on a cadence, not per frame: the reply costs a
            // round-trip that would otherwise precede every grab, and external size
            // changes are rare (live resizes arrive through region_dirty instead).
            // The gate reads the live copy, so a region re-targeted to the root edge
            // starts following it from that point on. Grab-failure recovery enters
            // regardless of auto_adjust: a fixed-size region has no other re-clamp
            // path after an external root shrink, and without one the capture dies at
            // the failure cap instead of recovering.
            geometry_check = geometry_check.saturating_sub(1);
            if (rsettings.auto_adjust_screen_capture_size || grab_failures > 0)
                && geometry_check <= 0
            {
                geometry_check = GEOMETRY_POLL_FRAMES;
                if let Some(g) = conn.get_geometry(root).ok().and_then(|c| c.reply().ok()) {
                    // A root shrink can strand even a previously-valid offset past
                    // the new edge; re-clamp it against the live root.
                    root_w = g.width;
                    root_h = g.height;
                    cap_x = clamp_offset(rsettings.capture_x, root_w);
                    cap_y = clamp_offset(rsettings.capture_y, root_h);
                    let (nw, nh) = resolve_dims(g.width, g.height, &rsettings);
                    if nw != cap_w || nh != cap_h {
                        if !pool.drain_for_resize(POOL_N, &controls.stop) {
                            break;
                        }
                        let (fw, fh) = conn
                            .get_geometry(root)
                            .ok()
                            .and_then(|c| c.reply().ok())
                            .map(|g| resolve_dims(g.width, g.height, &rsettings))
                            .unwrap_or((nw, nh));
                        if fw != cap_w || fh != cap_h {
                            for s in surfaces.iter_mut() {
                                s.destroy(&conn);
                            }
                            surfaces.clear();
                            cap_w = fw;
                            cap_h = fh;
                            for _ in 0..POOL_N {
                                surfaces.push(ShmSurface::create(&conn, cap_w, cap_h)?);
                            }
                            pool.bump_generation();
                        }
                    }
                }
            }

            let idx = match pool.acquire(&controls.stop) {
                Some(i) => i,
                None => break,
            };
            if let Some(d) = damage.as_ref() {
                d.clear(&conn);
            }
            let grab_result = {
                let surface = &mut surfaces[idx];
                grab_frame(&conn, root, surface, cap_x, cap_y, cap_w, cap_h)
            };
            if let Err(e) = grab_result {
                // Most likely an external root resize between geometry polls made
                // the grab run past the root edge: return the surface, re-poll
                // geometry immediately, and only give up if it never recovers.
                pool.recycle(idx);
                grab_failures += 1;
                geometry_check = 0;
                if grab_failures > 120 {
                    // A burn-out usually means the X connection itself is gone
                    // (Xorg restart, VT switch): every request fails until the
                    // server is back. Rebuild the channel a few times before
                    // declaring the capture dead — a recovered server resumes the
                    // stream where a dead thread would leave a black screen until
                    // the Python watchdog restarts everything.
                    let mut recovered = false;
                    for _attempt in 0..5 {
                        if controls.stop.load(Ordering::Relaxed) {
                            break;
                        }
                        std::thread::sleep(Duration::from_secs(1));
                        match try_rebuild_channel(
                            &mut conn,
                            &mut root,
                            &rsettings,
                            &mut cap_x,
                            &mut cap_y,
                            &mut cap_w,
                            &mut cap_h,
                            &mut root_w,
                            &mut root_h,
                            POOL_N,
                            &pool,
                            &mut surfaces,
                            &controls.stop,
                        ) {
                            Ok(()) => {
                                // The reported region belonged to the server that went away.
                                damage = RootDamage::create(&conn, root);
                                recovered = true;
                                break;
                            }
                            Err(re) => {
                                eprintln!("[X11] reconnect attempt failed: {re}");
                            }
                        }
                    }
                    if !recovered {
                        return Err(format!("capture could not recover: {e}"));
                    }
                    grab_failures = 0;
                    geometry_check = 0;
                    reconnect_streak += 1;
                    if reconnect_streak >= 4 {
                        return Err(
                            "capture recovered its channel but the region never grabbed again"
                                .to_string(),
                        );
                    }
                }
                continue;
            }
            grab_failures = 0;
            reconnect_streak = 0;
            let surface = &mut surfaces[idx];

            let frame_w = cap_w as i32;
            let frame_h = cap_h as i32;
            let stride = surface.stride;
            let buf = surface.as_mut_slice();

            if controls.capture_cursor.load(Ordering::Relaxed)
                && let Some(c) = conn
                    .xfixes_get_cursor_image()
                    .ok()
                    .and_then(|c| c.reply().ok())
                && c.width > 0
                && c.height > 0
            {
                // Translate from root coordinates using the LIVE grab origin
                // (cap_x/cap_y follow region_dirty pans and clamp negatives
                // exactly like the grab itself), never the immutable startup
                // settings — those go stale on the first live region move.
                let (img_x, img_y) =
                    cursor_image_origin(c.x, c.y, c.xhot, c.yhot, cap_x as i32, cap_y as i32);
                overlay_cursor(
                    buf,
                    stride,
                    frame_w,
                    frame_h,
                    c.width as i32,
                    c.height as i32,
                    &c.cursor_image,
                    img_x,
                    img_y,
                );
            }

            if watermark.is_active() {
                watermark.update_position(frame_w, frame_h, settings.watermark_location_enum);
                watermark.blend_bgra(buf, stride, frame_w, frame_h);
            }

            let published = pool.publish(
                RawFrame {
                    idx,
                    ptr: surface.addr,
                    len: surface.size,
                    width: cap_w,
                    height: cap_h,
                    stride,
                    generation: pool.generation(),
                    captured_ns: crate::wayland::host::now_ns(),
                },
                &controls.stop,
            );
            if !published {
                break;
            }
        }
        Ok(())
    })();

    pool.shutdown();
    let _ = encode_thread.join();
    for s in surfaces.iter_mut() {
        s.destroy(&conn);
    }
    match result {
        // A real capture error is the more specific diagnosis, so it outranks the panic flag.
        Err(e) => Err(e),
        Ok(()) if encode_panicked.load(Ordering::Acquire) => Err("encode thread panicked".into()),
        Ok(()) => Ok(()),
    }
}

/// Helpers the hardware checks of the zero-copy backends share: a known picture on the root of
/// `$DISPLAY`, and the color a decoded frame of it must average to.
#[cfg(test)]
pub(crate) mod gpu_test_support {
    use std::time::Duration;

    use crate::RustCaptureSettings;
    use crate::encoders::codec::Codec;
    use crate::webcam::decode::{Decoder, VideoDecoder};

    /// Full-frame capture settings for `codec` at CRF 25, streaming every frame, on the render
    /// node `auto_gpu` picks first.
    pub(crate) fn settings(codec: Codec) -> RustCaptureSettings {
        RustCaptureSettings {
            codec,
            target_fps: 60.0,
            video_crf: 25,
            video_streaming_mode: true,
            encode_node_index: crate::auto_render_node("true")
                .and_then(|node| crate::render_node_index(&node))
                .unwrap_or(0),
            ..Default::default()
        }
    }

    /// Paint the whole root of `$DISPLAY` one solid color and let the server finish, so the
    /// next capture has a known picture in it.
    ///
    /// The screen saver is turned off first: a test display sees no input, so a server left
    /// with the default ten-minute blanking timeout hands the capture a black screen and every
    /// color comparison fails for a reason that has nothing to do with the capture.
    pub(crate) fn paint_root(rgb: (u8, u8, u8)) -> bool {
        let _ = std::process::Command::new("xset")
            .args(["s", "off", "s", "noblank"])
            .output();
        let _ = std::process::Command::new("xset")
            .arg("s")
            .arg("reset")
            .output();
        let spec = format!("#{:02x}{:02x}{:02x}", rgb.0, rgb.1, rgb.2);
        let out = std::process::Command::new("xsetroot")
            .args(["-solid", &spec])
            .output();
        if !out.map(|o| o.status.success()).unwrap_or(false) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(120));
        true
    }

    /// Limited-range Y/Cb/Cr of an 8-bit RGB triple, what the chroma convert emits for a
    /// captured surface.
    pub(crate) fn painted_ycbcr(rgb: (u8, u8, u8)) -> [f64; 3] {
        use crate::encoders::chroma_siting::{BT709, ycbcr};
        ycbcr([rgb.0, rgb.1, rgb.2].map(f64::from), BT709)
    }

    /// Mean Y/Cb/Cr of a decoded picture.
    pub(crate) fn decoded_mean(dec: &mut VideoDecoder, payload: &[u8]) -> [f64; 3] {
        assert!(
            dec.decode(payload).expect("decode"),
            "no picture from this access unit"
        );
        let v = dec.frame().expect("decoded frame");
        let mut acc = [0f64; 3];
        let mut n = 0f64;
        for y in 0..v.height {
            for x in 0..v.width {
                acc[0] += v.y[y * v.y_stride + x] as f64;
                acc[1] += v.u[(y / 2) * v.uv_stride + x / 2] as f64;
                acc[2] += v.v[(y / 2) * v.uv_stride + x / 2] as f64;
                n += 1.0;
            }
        }
        [acc[0] / n, acc[1] / n, acc[2] / n]
    }
}

#[cfg(test)]
mod pool_tests {
    use super::*;

    fn dummy(idx: usize) -> RawFrame {
        RawFrame {
            idx,
            ptr: std::ptr::null_mut(),
            len: 0,
            width: 0,
            height: 0,
            stride: 0,
            generation: 0,
            captured_ns: 0,
        }
    }

    /// A full acquire -> publish -> take -> recycle round-trip returns the same surface, and
    /// afterwards all three pool surfaces are acquirable again and distinct.
    #[test]
    fn roundtrip_then_recycle_returns_all_surfaces() {
        let p = FramePool::new(3);
        let stop = AtomicBool::new(false);
        let a = p.acquire(&stop).unwrap();
        assert!(p.publish(dummy(a), &stop));
        let f = p.take().unwrap();
        assert_eq!(f.idx, a);
        p.recycle(f.idx);
        let (x, y, z) = (
            p.acquire(&stop).unwrap(),
            p.acquire(&stop).unwrap(),
            p.acquire(&stop).unwrap(),
        );
        assert!(x != y && y != z && x != z);
    }

    /// With every surface held and `stop` set, `acquire` returns `None` after its bounded wait
    /// rather than blocking forever.
    #[test]
    fn acquire_returns_none_when_exhausted_and_stopped() {
        let p = FramePool::new(2);
        let stop = AtomicBool::new(false);
        let _a = p.acquire(&stop).unwrap();
        let _b = p.acquire(&stop).unwrap();
        stop.store(true, Ordering::Relaxed);
        assert!(p.acquire(&stop).is_none());
    }

    /// With the handoff slot already occupied and `stop` set, `publish` returns `false` (frame
    /// discarded) instead of blocking.
    #[test]
    fn publish_returns_false_when_slot_full_and_stopped() {
        let p = FramePool::new(3);
        let stop = AtomicBool::new(false);
        let a = p.acquire(&stop).unwrap();
        assert!(p.publish(dummy(a), &stop));
        let b = p.acquire(&stop).unwrap();
        stop.store(true, Ordering::Relaxed);
        assert!(!p.publish(dummy(b), &stop));
    }

    /// `drain_for_resize` blocks until every held surface has been recycled (`free == n`), then
    /// reports a full drain and leaves the pool whole; a helper thread recycles the three held surfaces
    /// shortly after the drain begins.
    #[test]
    fn drain_for_resize_waits_until_all_free() {
        let p = Arc::new(FramePool::new(3));
        let stop = AtomicBool::new(false);
        let held = [
            p.acquire(&stop).unwrap(),
            p.acquire(&stop).unwrap(),
            p.acquire(&stop).unwrap(),
        ];
        let p2 = p.clone();
        let t = thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            for idx in held {
                p2.recycle(idx);
            }
        });
        assert!(p.drain_for_resize(3, &stop));
        t.join().unwrap();
        assert!(p.acquire(&stop).is_some());
    }

    /// A surface is held and never recycled (as a dead encode thread would leave it): setting
    /// `stop` unblocks the bounded drain, which reports it did NOT fully drain instead of hanging.
    #[test]
    fn drain_for_resize_aborts_on_stop() {
        let p = FramePool::new(3);
        let stop = AtomicBool::new(false);
        let _held = p.acquire(&stop).unwrap();
        stop.store(true, Ordering::Relaxed);
        assert!(!p.drain_for_resize(3, &stop));
    }

    /// A W1 -> W2 -> W1 resize flap where the encoder never takes the W2 frame:
    /// `drain_for_resize` reclaims the un-taken frame from the slot, the surfaces recreate twice, and
    /// the next taken frame lands back on the ORIGINAL dimensions — so the bumped generation is the
    /// only rebuild signal that invalidates encoder state keyed to the reused surface addresses.
    #[test]
    fn resize_flap_reclaim_changes_generation_at_identical_dims() {
        let p = FramePool::new(3);
        let stop = AtomicBool::new(false);
        let a = p.acquire(&stop).unwrap();
        assert!(p.publish(
            RawFrame {
                generation: p.generation(),
                width: 1920,
                height: 1080,
                ..dummy(a)
            },
            &stop
        ));
        let f = p.take().unwrap();
        let (last_gen, last_dims) = (f.generation, (f.width, f.height));
        p.recycle(f.idx);
        assert!(p.drain_for_resize(3, &stop));
        p.bump_generation();
        let b = p.acquire(&stop).unwrap();
        assert!(p.publish(
            RawFrame {
                generation: p.generation(),
                width: 2560,
                height: 1600,
                ..dummy(b)
            },
            &stop
        ));
        assert!(p.drain_for_resize(3, &stop));
        p.bump_generation();
        let c = p.acquire(&stop).unwrap();
        assert!(p.publish(
            RawFrame {
                generation: p.generation(),
                width: 1920,
                height: 1080,
                ..dummy(c)
            },
            &stop
        ));
        let g = p.take().unwrap();
        assert_eq!(
            (g.width, g.height),
            last_dims,
            "flap lands on identical dimensions"
        );
        assert_eq!(g.generation, 2);
        assert_ne!(
            g.generation, last_gen,
            "generation is the only rebuild signal"
        );
    }

    /// Recreating the surfaces bumps the generation, and frames published afterwards carry the
    /// new value — the signal the encode thread uses to trigger a rebuild at identical dimensions.
    #[test]
    fn generation_bumps_on_recreate_and_rides_published_frames() {
        let p = FramePool::new(3);
        let stop = AtomicBool::new(false);
        assert_eq!(p.generation(), 0);
        let a = p.acquire(&stop).unwrap();
        assert!(p.publish(
            RawFrame {
                generation: p.generation(),
                ..dummy(a)
            },
            &stop
        ));
        let f = p.take().unwrap();
        assert_eq!(f.generation, 0);
        p.recycle(f.idx);
        p.bump_generation();
        let b = p.acquire(&stop).unwrap();
        assert!(p.publish(
            RawFrame {
                generation: p.generation(),
                ..dummy(b)
            },
            &stop
        ));
        assert_eq!(p.take().unwrap().generation, 1);
    }
}

#[cfg(test)]
mod cursor_tests {
    use super::*;

    /// `cursor_image_origin` subtracts both the hotspot and the capture-region offset from the
    /// reported pointer position, and goes negative when the hotspot sits near the frame origin (the
    /// result is clipped when drawn).
    #[test]
    fn origin_subtracts_hotspot_and_capture_offset() {
        assert_eq!(cursor_image_origin(100, 80, 4, 6, 0, 0), (96, 74));
        assert_eq!(cursor_image_origin(100, 80, 4, 6, 10, 20), (86, 54));
        assert_eq!(cursor_image_origin(1, 1, 8, 8, 0, 0), (-7, -7));
    }

    /// `overlay_cursor` blits at the hotspot-offset origin, not the raw pointer position: a
    /// 2x2 cursor with hotspot (1,1) at pointer (4,4) lands at (3,3)..(4,4), leaving the un-offset
    /// (hotspot) corner untouched.
    #[test]
    fn overlay_blits_at_hotspot_offset_origin() {
        let stride = 8 * 4;
        let mut frame = vec![0u8; stride * 8];
        let pixels = [0xFFFF_FFFFu32; 4];
        let (ox, oy) = cursor_image_origin(4, 4, 1, 1, 0, 0);
        overlay_cursor(&mut frame, stride, 8, 8, 2, 2, &pixels, ox, oy);
        let px = |x: usize, y: usize| frame[y * stride + x * 4];
        assert_eq!(px(3, 3), 255);
        assert_eq!(px(4, 4), 255);
        assert_eq!(px(2, 2), 0);
        assert_eq!(px(5, 5), 0, "no pixel at the un-offset (hotspot) corner");
    }

    #[test]
    fn offset_clamps_into_root() {
        assert_eq!(clamp_offset(100, 1920), 100);
        assert_eq!(clamp_offset(-50, 1920), 0);
        // Past-end offsets keep one root pixel instead of failing forever.
        assert_eq!(clamp_offset(100000, 1920), 1919);
        // A protocol-max root still maps into i16 X coordinates.
        assert_eq!(clamp_offset(40000, u16::MAX), 32766);
    }

    #[test]
    fn resolve_dims_survives_past_end_offset() {
        let s = RustCaptureSettings {
            capture_x: 100000,
            capture_y: 100000,
            width: 1920,
            height: 1080,
            codec: crate::encoders::Codec::H264,
            ..Default::default()
        };
        let (w, h) = resolve_dims(1920, 1080, &s);
        assert!(
            w >= 2 && h >= 2,
            "a past-end offset now degrades to a small grab, not death"
        );
        assert_eq!(w % 2, 0);
        assert_eq!(h % 2, 0);
    }

    /// `overlay_cursor` treats XFixes pixels as premultiplied: half-alpha at half
    /// intensity stays at that intensity over black (the straight-alpha formula
    /// would multiply alpha twice and darken it).
    #[test]
    fn overlay_blends_premultiplied_pixels() {
        let stride = 4 * 4;
        let mut frame = vec![0u8; stride * 4];
        // a=128, g=128 (premultiplied half-intensity), b=64.
        let pixels = [0x8000_8040u32; 4];
        overlay_cursor(&mut frame, stride, 4, 4, 2, 2, &pixels, 0, 0);
        assert_eq!(frame[0], 64, "blue channel: dst = 64 over black");
        assert_eq!(
            frame[1], 128,
            "premultiplied half-alpha stays 128, not darkened to 64"
        );
    }

    /// `overlay_cursor` clips a negative origin at the frame edge: with the origin at (-1,-1)
    /// only the in-frame quadrant is written, landing the cursor's bottom-right pixel at (0,0).
    #[test]
    fn overlay_clips_negative_origin_at_frame_edge() {
        let stride = 4 * 4;
        let mut frame = vec![0u8; stride * 4];
        let pixels = [0xFFFF_FFFFu32; 4];
        let (ox, oy) = cursor_image_origin(0, 0, 1, 1, 0, 0);
        overlay_cursor(&mut frame, stride, 4, 4, 2, 2, &pixels, ox, oy);
        assert_eq!(frame[0], 255);
        assert!(
            frame[4..].iter().all(|&b| b == 0),
            "no writes outside (0,0)"
        );
    }
}

#[cfg(test)]
mod damage_tests {
    //! The `x11_damage` tests need an X server of their own, quiet enough that nothing else
    //! repaints it: `DISPLAY=:N cargo test x11_damage -- --ignored --nocapture`.

    use super::*;
    use crate::pipeline::Damage as Changed;
    use x11rb::protocol::xproto::CreateGCAux;

    const PERIOD: Duration = Duration::from_millis(400);

    fn fill(color: u32, x: i16, y: i16, width: u16, height: u16) {
        Painter::connect().fill(color, x, y, width, height);
    }

    /// A client of its own that paints the root, connected ahead of when it paints, so a paint
    /// timed within a period is not held up by the connection's setup.
    struct Painter {
        conn: RustConnection,
        root: u32,
    }

    impl Painter {
        fn connect() -> Self {
            let (conn, screen) = x11rb::connect(None).expect("connect");
            let root = conn.setup().roots[screen].root;
            Self { conn, root }
        }

        /// Paints and waits for the server to have done it: a request only flushed may be
        /// carried out after the test has moved on, and land in the next one.
        fn fill(&self, color: u32, x: i16, y: i16, width: u16, height: u16) {
            let gc = self.conn.generate_id().expect("id");
            self.conn
                .create_gc(gc, self.root, &CreateGCAux::new().foreground(color))
                .expect("gc");
            self.conn
                .poly_fill_rectangle(
                    self.root,
                    gc,
                    &[Rectangle {
                        x,
                        y,
                        width,
                        height,
                    }],
                )
                .expect("fill");
            self.conn.free_gc(gc).expect("free");
            self.conn
                .get_input_focus()
                .expect("sync")
                .reply()
                .expect("sync");
        }
    }

    /// The root's damaged rectangles read in the captured area's own coordinates, clipped to it.
    #[test]
    fn the_root_damage_reads_as_the_captured_share() {
        let caret = [Rectangle {
            x: 110,
            y: 60,
            width: 2,
            height: 20,
        }];
        assert_eq!(
            captured_damage(&caret, 100, 50, 200, 100),
            Changed::Area(0.002)
        );
        assert_eq!(
            captured_damage(&caret, 120, 50, 200, 100),
            Changed::None,
            "left of the captured area"
        );
        let wide = [Rectangle {
            x: 0,
            y: 0,
            width: 1000,
            height: 1000,
        }];
        assert_eq!(
            captured_damage(&wide, 100, 50, 200, 100),
            Changed::Area(1.0)
        );
        assert_eq!(captured_damage(&[], 0, 0, 200, 100), Changed::None);
    }

    /// A caret-sized repaint reads as that small a share of the screen, and one outside the
    /// captured area as none of it.
    #[test]
    #[ignore]
    fn x11_damage_says_where_the_root_changed() {
        let (conn, screen) = x11rb::connect(None).expect("connect");
        let root = conn.setup().roots[screen].root;
        let (w, h) = (
            i32::from(conn.setup().roots[screen].width_in_pixels),
            i32::from(conn.setup().roots[screen].height_in_pixels),
        );
        let damage = RootDamage::create(&conn, root).expect("the server carries DAMAGE");
        damage.clear(&conn);
        assert!(
            damage.take(&conn).is_some_and(|rects| rects.is_empty()),
            "nothing reported yet"
        );
        let mut pace = FramePace::windowed();
        pace.ticked(TickTrigger::Timer, PERIOD, Instant::now(), false);
        fill(0x00ff_ffff, 300, 200, 2, 20);
        assert_eq!(
            wait_for_frame(&conn, Some(&damage), &pace, PERIOD),
            TickTrigger::Damage
        );
        let asked = Instant::now();
        let rects = damage.take(&conn).expect("the server says where");
        let took = asked.elapsed();
        let caret = captured_damage(&rects, 0, 0, w, h);
        assert_eq!(
            caret,
            Changed::Area((40.0 / (w * h) as f64) as f32),
            "{rects:?}"
        );
        assert_eq!(
            captured_damage(&rects, 400, 0, w - 400, h),
            Changed::None,
            "right of the caret"
        );
        println!("a 2x20 caret on a {w}x{h} root reads as {caret:?}, read in {took:?}");
    }

    /// A change reported after the wait gave up, before the tick takes the report, is the
    /// tick's: taken and called clean, it would be cleared and never sent.
    #[test]
    #[ignore]
    fn x11_damage_a_change_after_the_wait_is_the_ticks() {
        let (conn, screen) = x11rb::connect(None).expect("connect");
        let root = conn.setup().roots[screen].root;
        let damage = RootDamage::create(&conn, root).expect("the server carries DAMAGE");
        let mut pace = FramePace::windowed();
        pace.ticked(TickTrigger::Timer, PERIOD, Instant::now(), false);
        damage.clear(&conn);
        let trigger = wait_for_frame(&conn, Some(&damage), &pace, PERIOD);
        assert_eq!(trigger, TickTrigger::Timer, "a quiet screen is the timer's");
        fill(0x0040_8020, 10, 10, 8, 8);
        let (dirty, rects) = take_tick(&damage, &conn, trigger);
        assert!(dirty, "the late change is the tick's: {rects:?}");
        assert!(
            rects.is_some_and(|rects| !rects.is_empty()),
            "and says where"
        );
        let (dirty, rects) = take_tick(&damage, &conn, TickTrigger::Timer);
        assert!(!dirty, "taken once: {rects:?}");
    }

    /// A change right after a frame is published as it lands, not held back half a period: the
    /// frame it would have waited for is the window's, and no change had taken it.
    #[test]
    #[ignore]
    fn x11_damage_publishes_a_change_right_after_a_frame() {
        let (conn, screen) = x11rb::connect(None).expect("connect");
        let root = conn.setup().roots[screen].root;
        let damage = RootDamage::create(&conn, root).expect("the server carries DAMAGE");
        let mut pace = FramePace::windowed();
        pace.record_tick(TickTrigger::Timer, PERIOD, Instant::now(), true);
        damage.clear(&conn);
        let brush = Painter::connect();
        let painter = thread::spawn(move || {
            thread::sleep(PERIOD.mul_f64(0.1));
            brush.fill(0x0060_2040, 0, 0, 64, 64);
        });
        let start = Instant::now();
        let pulled = wait_for_frame(&conn, Some(&damage), &pace, PERIOD);
        let waited = start.elapsed();
        painter.join().expect("painter");
        assert_eq!(pulled, TickTrigger::Damage);
        assert!(
            waited < PERIOD.mul_f64(0.35),
            "a change a tenth of a period after a frame was published after {waited:?}"
        );
    }

    /// A screen that changes mid-period publishes the change then, while a screen that does not
    /// keeps the cadence: the latency of a change is its own, not the frame rate's.
    #[test]
    #[ignore]
    fn x11_damage_publishes_a_change_before_the_next_tick() {
        let (conn, screen) = x11rb::connect(None).expect("connect");
        let root = conn.setup().roots[screen].root;
        let damage = RootDamage::create(&conn, root).expect("the server carries DAMAGE");
        let mut pace = FramePace::windowed();

        pace.ticked(TickTrigger::Timer, PERIOD, Instant::now(), false);
        damage.clear(&conn);
        let start = Instant::now();
        let quiet = wait_for_frame(&conn, Some(&damage), &pace, PERIOD);
        let waited = start.elapsed();
        assert_eq!(
            quiet,
            TickTrigger::Timer,
            "a quiet screen is the timer's to pace"
        );
        assert!(
            waited >= PERIOD.mul_f64(0.9),
            "the timer's own tick came after {waited:?}"
        );

        pace.ticked(TickTrigger::Timer, PERIOD, Instant::now(), false);
        damage.clear(&conn);
        let brush = Painter::connect();
        let painter = thread::spawn(move || {
            thread::sleep(PERIOD.mul_f64(0.6));
            brush.fill(0x0020_4060, 0, 0, 64, 64);
        });
        let start = Instant::now();
        let pulled = wait_for_frame(&conn, Some(&damage), &pace, PERIOD);
        let waited = start.elapsed();
        painter.join().expect("painter");
        assert_eq!(
            pulled,
            TickTrigger::Damage,
            "the repaint is what asked for the frame"
        );
        assert!(
            waited < PERIOD.mul_f64(0.9),
            "the repaint was published after {waited:?}"
        );
        println!(
            "repaint at 0.6 of a period published after {:.0}ms",
            waited.as_secs_f64() * 1000.0
        );
    }
}

#[cfg(test)]
mod vblank_tests {
    //! Needs an X server of its own: `DISPLAY=:N cargo test x11_vblank -- --ignored`.

    use super::*;

    fn capture_at(fps: f64) -> Arc<Controls> {
        Arc::new(Controls::new(&RustCaptureSettings {
            target_fps: fps,
            ..Default::default()
        }))
    }

    /// The root window's `_FAKE_SCREEN_FPS` and `_FAKE_SCREEN_MILLIHZ`.
    fn published() -> (Option<u32>, Option<u32>) {
        (
            property(b"_FAKE_SCREEN_FPS"),
            property(b"_FAKE_SCREEN_MILLIHZ"),
        )
    }

    fn property(name: &[u8]) -> Option<u32> {
        let (conn, screen) = x11rb::connect(None).expect("connect");
        let root = conn.setup().roots[screen].root;
        let atom = conn
            .intern_atom(false, name)
            .expect("intern")
            .reply()
            .expect("atom")
            .atom;
        let reply = conn
            .get_property(false, root, atom, AtomEnum::CARDINAL, 0, 1)
            .expect("get_property")
            .reply()
            .expect("reply");
        reply.value32().and_then(|mut v| v.next())
    }

    /// The fastest running capture sets the rate, a change of rate follows, a fractional one to the
    /// millihertz, and the last capture to end takes the properties away.
    #[test]
    #[ignore]
    fn x11_vblank_rate_follows_the_fastest_capture() {
        let fast = capture_at(144.0);
        let slow = capture_at(90.0);
        let fast_claim = VblankClaim::new(&fast);
        let slow_claim = VblankClaim::new(&slow);
        assert_eq!(published(), (Some(144), Some(144_000)));
        drop(fast_claim);
        assert_eq!(published(), (Some(90), Some(90_000)));
        slow.fps_bits.store(119.6f64.to_bits(), Ordering::Relaxed);
        follow_frame_rate();
        assert_eq!(published(), (Some(120), Some(119_600)));
        slow.fps_bits
            .store((60000.0f64 / 1001.0).to_bits(), Ordering::Relaxed);
        follow_frame_rate();
        assert_eq!(published(), (Some(60), Some(59_940)));
        drop(slow_claim);
        assert_eq!(published(), (None, None));
    }
}
