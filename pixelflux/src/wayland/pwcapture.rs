/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! PipeWire consumer for a portal monitor stream: one `Video/Input` stream per host output on
//! the connection the portal opened, delivering the compositor's frames to the host-capture
//! consumer as [`HostFrame`]s.
//!
//! The compositor allocates and fills the buffers, so a frame is never copied here: a dmabuf
//! buffer becomes a `Dmabuf` the encoder imports in place (built once per buffer from
//! duplicated fds, so the encoder's import cache keeps hitting), a memfd buffer is mapped once
//! and read in place by the CPU path. A buffer stays dequeued from the moment its frame is
//! published until the consumer releases it, which is the same slot discipline the Wayland
//! capture threads follow, so the retained frame an IDR re-encodes is still the compositor's
//! memory. Frames are handed on from the stream's own thread the instant `process` runs, and
//! the format offered to the compositor asks it to send at most the capture's rate, so nothing
//! is captured only to be dropped and nothing waits for a timer.
//!
//! Format negotiation follows the modifier convention consumers and compositors agree on: each
//! importable `(fourcc, modifiers)` pair the encoder's EGL display reported is one format entry
//! with a mandatory, non-fixating modifier choice, followed by plain entries the compositor
//! answers with memfd buffers; the compositor picks, and the fixated format decides whether
//! dmabuf or memfd buffers are requested. Cursor metadata is requested so a metadata-mode
//! session delivers the host's real cursor sprite to the cursor callback, and damage metadata
//! so unchanged stripes are recognized.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::os::fd::{FromRawFd, OwnedFd};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicPtr, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use smithay::backend::allocator::dmabuf::{Dmabuf, DmabufFlags};
use smithay::backend::allocator::{Fourcc, Modifier};
use smithay::utils::{Physical, Rectangle};

use crate::pipewire::*;
use crate::wayland::cursor::CursorJob;
use crate::wayland::host::{FrameSink, HostCpuFrame, HostFrame, now_ns, trace};

const FOURCC_XR24: u32 = 0x3432_5258;
const FOURCC_AR24: u32 = 0x3432_5241;
const FOURCC_XB24: u32 = 0x3432_4258;
const FOURCC_AB24: u32 = 0x3432_4241;
const MAX_DAMAGE_REGIONS: u32 = 16;
/// The largest cursor sprite edge the cursor meta is asked to hold. A compositor offers that
/// meta at one fixed size (Mutter 384x384, KWin 256x256), and PipeWire leaves it off the
/// buffers, with every sprite, unless the size falls inside the range asked for.
const MAX_CURSOR_EDGE: u32 = 1024;

/// The SPA pixel formats a consumer of BGRA-order pixels takes, best first, with the DRM fourcc
/// each corresponds to (SPA names byte order, DRM names the little-endian word).
const SHM_FORMATS: [(u32, u32); 4] = [
    (SPA_VIDEO_FORMAT_BGRX, FOURCC_XR24),
    (SPA_VIDEO_FORMAT_BGRA, FOURCC_AR24),
    (SPA_VIDEO_FORMAT_RGBX, FOURCC_XB24),
    (SPA_VIDEO_FORMAT_RGBA, FOURCC_AB24),
];

fn spa_to_fourcc(spa: u32) -> Option<u32> {
    SHM_FORMATS.iter().find(|(s, _)| *s == spa).map(|(_, f)| *f)
}

/// The wl_shm-style code `HostCpuFrame` converts by: 0/1 for the ARGB/XRGB words, DRM fourcc
/// for the rest.
fn shm_code(fourcc: u32) -> u32 {
    match fourcc {
        FOURCC_AR24 => 0,
        FOURCC_XR24 => 1,
        other => other,
    }
}

/// One PipeWire connection, shared by every stream of a portal session.
pub struct PwCore {
    api: Api,
    thread_loop: *mut c_void,
    context: *mut c_void,
    core: *mut c_void,
}

unsafe impl Send for PwCore {}
unsafe impl Sync for PwCore {}

impl PwCore {
    /// Connect on the fd the portal handed out (PipeWire owns and closes it).
    pub fn connect(fd: OwnedFd) -> Result<Arc<Self>, String> {
        let api = *api()?;
        unsafe {
            let thread_loop = (api.thread_loop_new)(c"pixelflux-portal-pw".as_ptr(), ptr::null());
            if thread_loop.is_null() {
                return Err("pw_thread_loop_new failed".into());
            }
            let mut this = PwCore {
                api,
                thread_loop,
                context: ptr::null_mut(),
                core: ptr::null_mut(),
            };
            this.context =
                (api.context_new)((api.thread_loop_get_loop)(thread_loop), ptr::null_mut(), 0);
            if this.context.is_null() {
                return Err("pw_context_new failed".into());
            }
            if (api.thread_loop_start)(thread_loop) < 0 {
                return Err("pw_thread_loop_start failed".into());
            }
            (api.thread_loop_lock)(thread_loop);
            this.core = (api.context_connect_fd)(
                this.context,
                std::os::fd::IntoRawFd::into_raw_fd(fd),
                ptr::null_mut(),
                0,
            );
            (api.thread_loop_unlock)(thread_loop);
            if this.core.is_null() {
                return Err("pw_context_connect_fd failed".into());
            }
            Ok(Arc::new(this))
        }
    }

    /// Run `f` with the loop locked, which every stream call from outside the loop needs.
    fn locked<R>(&self, f: impl FnOnce() -> R) -> R {
        unsafe { (self.api.thread_loop_lock)(self.thread_loop) };
        let r = f();
        unsafe { (self.api.thread_loop_unlock)(self.thread_loop) };
        r
    }
}

impl Drop for PwCore {
    fn drop(&mut self) {
        unsafe {
            (self.api.thread_loop_lock)(self.thread_loop);
            if !self.core.is_null() {
                (self.api.core_disconnect)(self.core);
            }
            (self.api.thread_loop_unlock)(self.thread_loop);
            (self.api.thread_loop_stop)(self.thread_loop);
            if !self.context.is_null() {
                (self.api.context_destroy)(self.context);
            }
            (self.api.thread_loop_destroy)(self.thread_loop);
        }
    }
}

/// What a stream asks the compositor for.
#[derive(Clone, Debug, PartialEq)]
pub struct StreamConfig {
    /// Offer dmabuf import (with `dma_formats`); off, only memfd frames are offered for.
    pub zero_copy: bool,
    /// `(fourcc, modifiers)` the encoder's display imports.
    pub dma_formats: Vec<(u32, Vec<u64>)>,
    /// Rate cap the compositor is asked to hold, in millihertz.
    pub fps_milli: u32,
    /// The size the consumer expects (the preferred value of the size range).
    pub expect: (i32, i32),
    /// Where the stream's cursor metas land in the union layout (`HOST_POINTER`). Not a
    /// format matter: a change alone never renegotiates.
    pub place: CursorPlace,
}

/// Where a stream's output sits for its cursor metas: the display's origin in the union layout
/// and its capture size, as Selkies laid it out (`HostSession::set_layout`), and the output's
/// logical size. A zero size is unknown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CursorPlace {
    pub origin: (i32, i32),
    pub size: (i32, i32),
    pub logical: (i32, i32),
}

/// Host capture's pointer, for `WaylandBackend.pointer_location`: None without a host session;
/// while one is attached, the newest cursor meta's position, or None before the first and
/// where the host paints the cursor into its frames (no metas). The compositor's own
/// `PointerShare` is not read meanwhile: it stops at its last pass before the host attached.
/// Process-wide like the backend slot.
pub static HOST_POINTER: Mutex<Option<Option<crate::wayland::frontend::PointerAt>>> =
    Mutex::new(None);
/// The motion numbers handed to the host session, each with when it was sent (CLOCK_MONOTONIC
/// ns), newest last: a cursor meta claims the moves sent before its buffer was stamped
/// (`motion_at`), so one from a frame captured ahead of the latest move does not claim it.
static HOST_MOTIONS: Mutex<std::collections::VecDeque<(u64, i64)>> =
    Mutex::new(std::collections::VecDeque::new());
/// Sent moves remembered for `motion_at`: well over a frame's worth at any input rate.
const HOST_MOTIONS_KEPT: usize = 256;

/// Note motion number `motion` handed to the host session now.
pub fn note_host_motion(motion: u64) {
    let mut sent = HOST_MOTIONS.lock().unwrap();
    if sent.back().is_some_and(|&(last, _)| last >= motion) {
        return;
    }
    if sent.len() == HOST_MOTIONS_KEPT {
        sent.pop_front();
    }
    sent.push_back((motion, now_ns()));
}

/// The newest motion number of `sent` handed over at or before `stamp`; 0 before any.
fn motion_at(sent: &std::collections::VecDeque<(u64, i64)>, stamp: i64) -> u64 {
    sent.iter()
        .rev()
        .find(|&&(_, at)| at <= stamp)
        .map_or(0, |&(motion, _)| motion)
}

/// When a buffer was captured, for the moves its cursor meta claims: the compositor's stamp
/// where it is plausible (no later than `arrival`, at most four periods older), else `arrival`.
fn meta_stamp(pts: Option<i64>, arrival: i64, period_ns: i64) -> i64 {
    pts.filter(|&pts| pts <= arrival && arrival - pts <= STAMP_MAX_AGE_PERIODS * period_ns)
        .unwrap_or(arrival)
}

/// A cursor meta's position `(x, y)` in a stream negotiated at `stream` pixels as the pointer
/// in the union layout: the display's origin plus the position scaled to its capture size,
/// the inverse of the mapping absolute motion is injected through (`host::stream_point`).
/// The scale is the output's capture pixels per logical pixel, what a relative pixel moves.
pub(crate) fn host_pointer_at(
    place: CursorPlace,
    stream: (i32, i32),
    x: i32,
    y: i32,
    motion: u64,
) -> crate::wayland::frontend::PointerAt {
    let ratio = |to: i32, from: i32| {
        if to > 0 && from > 0 {
            f64::from(to) / f64::from(from)
        } else {
            1.0
        }
    };
    crate::wayland::frontend::PointerAt {
        x: f64::from(place.origin.0) + f64::from(x) * ratio(place.size.0, stream.0),
        y: f64::from(place.origin.1) + f64::from(y) * ratio(place.size.1, stream.1),
        scale: ratio(place.size.0, place.logical.0),
        motion,
    }
}

/// The pointer `WaylandBackend.pointer_location` reports: under host capture (`Some`), host
/// capture's alone, None included; otherwise the compositor's own.
pub fn prefer_host_pointer<T>(
    host: Option<Option<T>>,
    share: impl FnOnce() -> Option<T>,
) -> Option<T> {
    match host {
        Some(at) => at,
        None => share(),
    }
}

/// Publish a cursor meta's stream-pixel position to `HOST_POINTER` while a host session holds it,
/// claiming the moves up to `motion`.
fn publish_host_pointer(shared: &Shared, x: i32, y: i32, motion: u64) {
    let place = shared.cfg.lock().unwrap().place;
    let stream = shared
        .negotiated
        .lock()
        .unwrap()
        .map_or((0, 0), |n| (n.width, n.height));
    let at = host_pointer_at(place, stream, x, y, motion);
    if let Some(slot) = HOST_POINTER.lock().unwrap().as_mut() {
        *slot = Some(at);
    }
}

/// The fixated format of a stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Negotiated {
    pub width: i32,
    pub height: i32,
    pub fourcc: u32,
    pub modifier: Option<u64>,
}

/// One buffer the compositor handed the stream: its PipeWire handle plus the import built
/// on first use, kept for the buffer's life so the same fds reach the encoder each time.
#[derive(Default)]
struct BufferSlot {
    pw: *mut PwBuffer,
    dmabuf: Option<Dmabuf>,
    cpu: Option<Arc<memmap2::Mmap>>,
    /// Published to the consumer and not yet released: PipeWire does not own it meanwhile.
    held: bool,
}

/// State shared with the stream callbacks (handed to PipeWire as the listener's user data).
struct Shared {
    api: Api,
    stream: AtomicPtr<c_void>,
    sink: FrameSink,
    cursor_tx: std::sync::mpsc::Sender<CursorJob>,
    cfg: Mutex<StreamConfig>,
    negotiated: Mutex<Option<Negotiated>>,
    buffers: Mutex<Vec<BufferSlot>>,
    /// Bumped on every renegotiation; a release naming an older generation is ignored.
    generation: AtomicU64,
    state: AtomicI32,
    errored: AtomicBool,
    frames: AtomicU64,
    stamps: Mutex<Stamps>,
    cursor_pos: AtomicI64,
    cursor_hash: AtomicU64,
    /// The pods handed to `pw_stream_update_params`, alive for the stream's life.
    params: Mutex<Vec<Vec<u8>>>,
}

// The raw PipeWire buffer pointers in the slots are only dereferenced on the stream's own loop
// or under its lock; everything else in the state is atomics and mutexes.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

fn enum_format_pod(spa_format: u32, modifiers: Option<&[u64]>, cfg: &StreamConfig) -> Vec<u8> {
    object(SPA_TYPE_OBJECT_FORMAT, SPA_PARAM_ENUM_FORMAT, |p| {
        prop(p, SPA_FORMAT_MEDIA_TYPE, |v| {
            pod_id(v, SPA_MEDIA_TYPE_VIDEO)
        });
        prop(p, SPA_FORMAT_MEDIA_SUBTYPE, |v| {
            pod_id(v, SPA_MEDIA_SUBTYPE_RAW)
        });
        prop(p, SPA_FORMAT_VIDEO_FORMAT, |v| pod_id(v, spa_format));
        if let Some(mods) = modifiers {
            let mut values: Vec<i64> = Vec::with_capacity(mods.len() + 1);
            values.push(mods[0] as i64);
            values.extend(mods.iter().map(|m| *m as i64));
            prop_flags(
                p,
                SPA_FORMAT_VIDEO_MODIFIER,
                SPA_POD_PROP_FLAG_MANDATORY | SPA_POD_PROP_FLAG_DONT_FIXATE,
                |v| pod_choice_long(v, SPA_CHOICE_ENUM, &values),
            );
        }
        let (w, h) = (cfg.expect.0.max(1) as u32, cfg.expect.1.max(1) as u32);
        prop(p, SPA_FORMAT_VIDEO_SIZE, |v| {
            pod_choice_rect(v, SPA_CHOICE_RANGE, &[(w, h), (1, 1), (16384, 16384)])
        });
        prop(p, SPA_FORMAT_VIDEO_FRAMERATE, |v| pod_frac(v, 0, 1));
        let fps = (cfg.fps_milli.max(1000), 1000);
        prop(p, SPA_FORMAT_VIDEO_MAX_FRAMERATE, |v| {
            pod_choice_frac(v, SPA_CHOICE_RANGE, &[fps, (1, 1), fps])
        });
    })
}

/// The `EnumFormat` list for `cfg`: modifier-bearing entries for every importable fourcc when
/// dmabufs are wanted, then the plain entries every compositor answers with memfd buffers.
fn enum_format_pods(cfg: &StreamConfig) -> Vec<Vec<u8>> {
    let mut pods = Vec::new();
    if cfg.zero_copy {
        for (fourcc, mods) in &cfg.dma_formats {
            let spa = match *fourcc {
                FOURCC_XR24 => SPA_VIDEO_FORMAT_BGRX,
                FOURCC_AR24 => SPA_VIDEO_FORMAT_BGRA,
                _ => continue,
            };
            if !mods.is_empty() {
                pods.push(enum_format_pod(spa, Some(mods), cfg));
            }
        }
    }
    for (spa, _) in SHM_FORMATS {
        pods.push(enum_format_pod(spa, None, cfg));
    }
    pods
}

/// Buffers and metadata asked for once the format is fixated: dmabuf or memfd blocks per the
/// modifier's presence, three to eight buffers so one can be held retained while another is
/// in flight and the compositor still has one to fill, plus header, damage, and cursor metas.
fn negotiated_params(n: &Negotiated, dmabuf: bool) -> Vec<Vec<u8>> {
    let data_type = 1i32
        << if dmabuf {
            SPA_DATA_DMABUF
        } else {
            SPA_DATA_MEMFD
        };
    let buffers = object(SPA_TYPE_OBJECT_PARAM_BUFFERS, SPA_PARAM_BUFFERS, |p| {
        prop(p, SPA_PARAM_BUFFERS_BUFFERS, |v| {
            pod_choice_int(v, SPA_CHOICE_RANGE, &[4, 3, 8])
        });
        prop(p, SPA_PARAM_BUFFERS_BLOCKS, |v| pod_int(v, 1));
        prop(p, SPA_PARAM_BUFFERS_SIZE, |v| {
            pod_int(v, n.width * n.height * 4)
        });
        prop(p, SPA_PARAM_BUFFERS_STRIDE, |v| pod_int(v, n.width * 4));
        prop(p, SPA_PARAM_BUFFERS_ALIGN, |v| pod_int(v, 16));
        prop(p, SPA_PARAM_BUFFERS_DATATYPE, |v| pod_int(v, data_type));
    });
    let region = SPA_META_REGION_SIZE as i32;
    vec![
        buffers,
        meta(SPA_META_HEADER, |v| pod_int(v, SPA_META_HEADER_SIZE as i32)),
        meta(SPA_META_VIDEO_DAMAGE, |v| {
            pod_choice_int(
                v,
                SPA_CHOICE_RANGE,
                &[4 * region, region, MAX_DAMAGE_REGIONS as i32 * region],
            )
        }),
        meta(SPA_META_VIDEO_CROP, |v| pod_int(v, region)),
        meta(SPA_META_CURSOR, |v| {
            pod_choice_int(
                v,
                SPA_CHOICE_RANGE,
                &[
                    cursor_meta_size(64, 64) as i32,
                    cursor_meta_size(1, 1) as i32,
                    cursor_meta_size(MAX_CURSOR_EDGE, MAX_CURSOR_EDGE) as i32,
                ],
            )
        }),
    ]
}

/// Whether moving a stream's ask from `cur` to `new` renegotiates (`reconfigure`): anything but
/// the preferred size does, and that only before a format stands.
fn renegotiates(cur: &StreamConfig, new: &StreamConfig, negotiated: bool) -> bool {
    let size_only = StreamConfig {
        expect: cur.expect,
        place: cur.place,
        ..new.clone()
    } == *cur;
    !size_only || !negotiated
}

/// A `Meta` param asking for one metadata type of the given size (a plain or ranged Int).
fn meta(ty: u32, size: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    object(SPA_TYPE_OBJECT_PARAM_META, SPA_PARAM_META, |p| {
        prop(p, SPA_PARAM_META_TYPE, |v| pod_id(v, ty));
        prop(p, SPA_PARAM_META_SIZE, size);
    })
}

/// The fixated `Format` param: pixel format, size, and whether a modifier (dmabuf) was agreed.
fn parse_format(pod: &[u8]) -> Option<Negotiated> {
    let (ty, _, props) = object_props(pod)?;
    if ty != SPA_TYPE_OBJECT_FORMAT {
        return None;
    }
    let spa = find_prop(&props, SPA_FORMAT_VIDEO_FORMAT)?.as_u32()?;
    let (w, h) = find_prop(&props, SPA_FORMAT_VIDEO_SIZE)?.as_pair()?;
    let modifier = find_prop(&props, SPA_FORMAT_VIDEO_MODIFIER)
        .and_then(|p| p.as_i64())
        .map(|m| m as u64);
    Some(Negotiated {
        width: w as i32,
        height: h as i32,
        fourcc: spa_to_fourcc(spa)?,
        modifier,
    })
}

unsafe extern "C" fn on_state_changed(
    data: *mut c_void,
    _old: c_int,
    new: c_int,
    error: *const c_char,
) {
    let shared = unsafe { &*(data as *const Shared) };
    shared.state.store(new, Ordering::Release);
    let name = match new {
        PW_STREAM_STATE_ERROR => "error",
        PW_STREAM_STATE_UNCONNECTED => "unconnected",
        PW_STREAM_STATE_PAUSED => "paused",
        PW_STREAM_STATE_STREAMING => "streaming",
        _ => "connecting",
    };
    let msg = if error.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(error) }
            .to_string_lossy()
            .into_owned()
    };
    eprintln!(
        "[HostCapture] output {}: portal stream {name} {msg}",
        shared.sink.index
    );
    if new == PW_STREAM_STATE_ERROR {
        shared.errored.store(true, Ordering::Release);
    }
}

unsafe extern "C" fn on_param_changed(data: *mut c_void, id: u32, param: *const c_void) {
    if id != SPA_PARAM_FORMAT {
        return;
    }
    let shared = unsafe { &*(data as *const Shared) };
    shared.generation.fetch_add(1, Ordering::AcqRel);
    if param.is_null() {
        *shared.negotiated.lock().unwrap() = None;
        return;
    }
    let Some(negotiated) = parse_format(unsafe { pod_bytes(param) }) else {
        eprintln!(
            "[HostCapture] output {}: unusable PipeWire format",
            shared.sink.index
        );
        return;
    };
    eprintln!(
        "[HostCapture] output {} portal stream: {}x{} {} ({})",
        shared.sink.index,
        negotiated.width,
        negotiated.height,
        String::from_utf8_lossy(&negotiated.fourcc.to_le_bytes()),
        match negotiated.modifier {
            Some(m) => format!("dmabuf, modifier {m:#x}"),
            None => "memfd".to_string(),
        }
    );
    let pods = negotiated_params(&negotiated, negotiated.modifier.is_some());
    *shared.negotiated.lock().unwrap() = Some(negotiated);
    let stream = shared.stream.load(Ordering::Acquire);
    if stream.is_null() {
        return;
    }
    let mut ptrs: Vec<*const c_void> = pods.iter().map(|p| p.as_ptr() as *const c_void).collect();
    unsafe { (shared.api.stream_update_params)(stream, ptrs.as_mut_ptr(), ptrs.len() as u32) };
    *shared.params.lock().unwrap() = pods;
}

unsafe extern "C" fn on_add_buffer(data: *mut c_void, b: *mut PwBuffer) {
    let shared = unsafe { &*(data as *const Shared) };
    let mut buffers = shared.buffers.lock().unwrap();
    let idx = buffers
        .iter()
        .position(|s| s.pw.is_null())
        .unwrap_or_else(|| {
            buffers.push(BufferSlot::default());
            buffers.len() - 1
        });
    buffers[idx] = BufferSlot {
        pw: b,
        ..BufferSlot::default()
    };
    unsafe { (*b).user_data = idx as *mut c_void };
}

unsafe extern "C" fn on_remove_buffer(data: *mut c_void, b: *mut PwBuffer) {
    let shared = unsafe { &*(data as *const Shared) };
    let idx = unsafe { (*b).user_data } as usize;
    if let Some(slot) = shared.buffers.lock().unwrap().get_mut(idx) {
        *slot = BufferSlot::default();
    }
}

/// Read the cursor meta of a buffer: position, and a changed sprite forwarded to the cursor
/// worker (an empty sprite hides it).
unsafe fn read_cursor(shared: &Shared, meta: &SpaMeta, motion: u64) {
    if meta.size < std::mem::size_of::<SpaMetaCursor>() as u32 || meta.data.is_null() {
        return;
    }
    let cursor = unsafe { &*(meta.data as *const SpaMetaCursor) };
    if cursor.id == 0 {
        return;
    }
    shared.cursor_pos.store(
        ((cursor.x as i64) << 32) | (cursor.y as u32 as i64),
        Ordering::Relaxed,
    );
    publish_host_pointer(shared, cursor.x, cursor.y, motion);
    if cursor.bitmap_offset < std::mem::size_of::<SpaMetaCursor>() as u32
        || cursor.bitmap_offset + std::mem::size_of::<SpaMetaBitmap>() as u32 > meta.size
    {
        return;
    }
    let bitmap = unsafe {
        &*((meta.data as *const u8).add(cursor.bitmap_offset as usize) as *const SpaMetaBitmap)
    };
    if bitmap.format == 0 {
        return;
    }
    let (w, h) = (bitmap.width, bitmap.height);
    if w == 0 || h == 0 || bitmap.offset == 0 {
        if shared.cursor_hash.swap(0, Ordering::Relaxed) != 0 {
            let _ = shared.cursor_tx.send(CursorJob::Hide);
        }
        return;
    }
    let stride = bitmap.stride.max(0) as usize;
    let end = cursor.bitmap_offset as usize
        + bitmap.offset as usize
        + stride * (h as usize - 1)
        + w as usize * 4;
    if end > meta.size as usize || w > MAX_CURSOR_EDGE || h > MAX_CURSOR_EDGE {
        return;
    }
    let pixels = unsafe {
        (meta.data as *const u8).add(cursor.bitmap_offset as usize + bitmap.offset as usize)
    };
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h as usize {
        let row = unsafe { std::slice::from_raw_parts(pixels.add(y * stride), w as usize * 4) };
        for px in row.as_chunks::<4>().0 {
            let (r, g, b, a) = match bitmap.format {
                SPA_VIDEO_FORMAT_BGRA | SPA_VIDEO_FORMAT_BGRX => (
                    px[2],
                    px[1],
                    px[0],
                    if bitmap.format == SPA_VIDEO_FORMAT_BGRX {
                        0xff
                    } else {
                        px[3]
                    },
                ),
                SPA_VIDEO_FORMAT_ARGB => (px[1], px[2], px[3], px[0]),
                SPA_VIDEO_FORMAT_ABGR => (px[3], px[2], px[1], px[0]),
                _ => (
                    px[0],
                    px[1],
                    px[2],
                    if bitmap.format == SPA_VIDEO_FORMAT_RGBX {
                        0xff
                    } else {
                        px[3]
                    },
                ),
            };
            rgba.extend_from_slice(&[r, g, b, a]);
        }
    }
    let hash = xxhash_rust::xxh3::xxh3_64(&rgba) ^ ((w as u64) << 32 | h as u64);
    let previous = shared.cursor_hash.swap(hash, Ordering::Relaxed);
    if previous != hash {
        if previous == 0 {
            eprintln!(
                "[HostCapture] output {}: portal cursor sprites arrive ({w}x{h}, hotspot {},{})",
                shared.sink.index, cursor.hot_x, cursor.hot_y
            );
        }
        let _ = shared.cursor_tx.send(CursorJob::Gles {
            hash,
            width: w as i32,
            height: h as i32,
            bytes: rgba,
            hot_x: cursor.hot_x,
            hot_y: cursor.hot_y,
        });
    }
}

/// The damage regions of a buffer, or the whole frame when the compositor sent none.
unsafe fn read_damage(buf: &SpaBuffer, width: i32, height: i32) -> Vec<Rectangle<i32, Physical>> {
    let mut damage = Vec::new();
    for i in 0..buf.n_metas as usize {
        let m = unsafe { &*buf.metas.add(i) };
        if m.type_ != SPA_META_VIDEO_DAMAGE || m.data.is_null() {
            continue;
        }
        let n = m.size as usize / std::mem::size_of::<SpaMetaRegion>();
        for j in 0..n {
            let r = unsafe { *(m.data as *const SpaMetaRegion).add(j) };
            if r.width == 0 || r.height == 0 {
                break;
            }
            damage.push(Rectangle::new(
                (r.x, r.y).into(),
                (r.width as i32, r.height as i32).into(),
            ));
        }
    }
    if damage.is_empty() {
        damage.push(Rectangle::new((0, 0).into(), (width, height).into()));
    }
    damage
}

/// How many frame periods before its arrival a compositor's stamp may lie and still be taken
/// as the frame's capture instant (`Stamps`).
const STAMP_MAX_AGE_PERIODS: i64 = 4;

/// Why a portal frame's capture instant is its arrival rather than the compositor's stamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StampRefusal {
    Missing,
    Ahead,
    Stale,
    Repeated,
}

impl StampRefusal {
    fn says(self) -> &'static str {
        match self {
            StampRefusal::Missing => "missing",
            StampRefusal::Ahead => "ahead of the frame's arrival",
            StampRefusal::Stale => "older than four frame periods at arrival",
            StampRefusal::Repeated => "no later than the last frame's",
        }
    }
}

/// The capture instant of each portal frame, which the encoded frame carries as `capture_ns`
/// into the RTP timestamp, abs-capture-time and pipeline_ms: the compositor's header stamp
/// where it reads as a `CLOCK_MONOTONIC` instant of this frame, else the frame's arrival. A
/// stamp is taken when it is no later than the arrival, at most `STAMP_MAX_AGE_PERIODS`
/// frame periods before it, and later than the last instant given, so a stamp reused from an
/// earlier frame, one on another clock's scale, or one ahead of now moves neither. Which source
/// each refusal falls back to is logged the first time it happens on a stream.
#[derive(Default)]
struct Stamps {
    last: i64,
    refusals_seen: u8,
}

impl Stamps {
    fn instant(
        &mut self,
        pts: Option<i64>,
        arrival: i64,
        period_ns: i64,
    ) -> (i64, Option<StampRefusal>) {
        let refusal = match pts {
            None => Some(StampRefusal::Missing),
            Some(pts) if pts > arrival => Some(StampRefusal::Ahead),
            Some(pts) if arrival - pts > STAMP_MAX_AGE_PERIODS * period_ns => {
                Some(StampRefusal::Stale)
            }
            Some(pts) if pts <= self.last => Some(StampRefusal::Repeated),
            Some(_) => None,
        };
        let at = match (pts, refusal) {
            (Some(pts), None) => pts,
            _ => arrival.max(self.last + 1),
        };
        self.last = at;
        (at, refusal)
    }

    /// Whether this refusal is the first of its kind on the stream.
    fn first(&mut self, refusal: StampRefusal) -> bool {
        let bit = 1 << refusal as u8;
        let first = self.refusals_seen & bit == 0;
        self.refusals_seen |= bit;
        first
    }
}

/// The compositor's stamp from the buffer's header meta, when it carries one.
fn header_pts(buf: &SpaBuffer) -> Option<i64> {
    (0..buf.n_metas as usize)
        .map(|i| unsafe { &*buf.metas.add(i) })
        .find(|m| m.type_ == SPA_META_HEADER && m.size >= SPA_META_HEADER_SIZE && !m.data.is_null())
        .map(|m| unsafe { (*(m.data as *const SpaMetaHeader)).pts })
        .filter(|pts| *pts > 0)
}

/// Build the slot's import on its first frame: the dmabuf planes as a `Dmabuf` of duplicated
/// fds, or the memfd mapped whole.
unsafe fn import_slot(slot: &mut BufferSlot, buf: &SpaBuffer, n: &Negotiated) -> bool {
    if slot.dmabuf.is_some() || slot.cpu.is_some() {
        return true;
    }
    let d0 = unsafe { &*buf.datas };
    if d0.type_ == SPA_DATA_DMABUF {
        let mut builder = Dmabuf::builder(
            (n.width, n.height),
            Fourcc::try_from(n.fourcc).unwrap_or(Fourcc::Xrgb8888),
            Modifier::from(n.modifier.unwrap_or(u64::from(Modifier::Invalid))),
            DmabufFlags::empty(),
        );
        for i in 0..buf.n_datas as usize {
            let d = unsafe { &*buf.datas.add(i) };
            let dup = unsafe { libc::dup(d.fd as c_int) };
            if dup < 0 {
                return false;
            }
            let chunk = unsafe { &*d.chunk };
            builder.add_plane(
                unsafe { OwnedFd::from_raw_fd(dup) },
                chunk.offset,
                chunk.stride as u32,
            );
        }
        slot.dmabuf = builder.build();
        slot.dmabuf.is_some()
    } else if d0.type_ == SPA_DATA_MEMFD {
        let file = unsafe { std::fs::File::from_raw_fd(libc::dup(d0.fd as c_int)) };
        let map = unsafe {
            memmap2::MmapOptions::new()
                .offset(d0.mapoffset as u64)
                .len(d0.maxsize as usize)
                .map(&file)
        };
        match map {
            Ok(m) => {
                slot.cpu = Some(Arc::new(m));
                true
            }
            Err(e) => {
                eprintln!("[HostCapture] memfd map: {e}");
                false
            }
        }
    } else {
        false
    }
}

unsafe extern "C" fn on_process(data: *mut c_void) {
    let shared = unsafe { &*(data as *const Shared) };
    let stream = shared.stream.load(Ordering::Acquire);
    if stream.is_null() {
        return;
    }
    let Some(n) = *shared.negotiated.lock().unwrap() else {
        return;
    };
    // Take everything queued: cursor-only buffers go straight back, and of several frames
    // only the newest is worth publishing.
    let arrival = now_ns();
    let period_ns = 1_000_000_000_000 / shared.cfg.lock().unwrap().fps_milli.max(1000) as i64;
    let mut newest: Option<ptr::NonNull<PwBuffer>> = None;
    loop {
        let b = unsafe { (shared.api.stream_dequeue_buffer)(stream) };
        if b.is_null() {
            break;
        }
        let buf = unsafe { &*(*b).buffer };
        for i in 0..buf.n_metas as usize {
            let m = unsafe { &*buf.metas.add(i) };
            if m.type_ == SPA_META_CURSOR {
                let stamp = meta_stamp(header_pts(buf), arrival, period_ns);
                let motion = motion_at(&HOST_MOTIONS.lock().unwrap(), stamp);
                unsafe { read_cursor(shared, m, motion) };
            }
        }
        let has_video = buf.n_datas > 0 && {
            let d = unsafe { &*buf.datas };
            let chunk = unsafe { &*d.chunk };
            chunk.size > 0 && chunk.flags & SPA_CHUNK_FLAG_CORRUPTED == 0
        };
        if has_video {
            if let Some(previous) = newest {
                unsafe { (shared.api.stream_queue_buffer)(stream, previous.as_ptr()) };
            }
            newest = ptr::NonNull::new(b);
        } else {
            unsafe { (shared.api.stream_queue_buffer)(stream, b) };
        }
    }
    let Some(newest) = newest.map(|p| p.as_ptr()) else {
        return;
    };
    let buf = unsafe { &*(*newest).buffer };
    let idx = unsafe { (*newest).user_data } as usize;
    let generation = shared.generation.load(Ordering::Acquire);
    let pts = header_pts(buf);
    let arrival = now_ns();
    let (stamp_ns, refusal) = shared
        .stamps
        .lock()
        .unwrap()
        .instant(pts, arrival, period_ns);
    let frame = {
        let mut buffers = shared.buffers.lock().unwrap();
        let Some(slot) = buffers.get_mut(idx).filter(|s| s.pw == newest) else {
            unsafe { (shared.api.stream_queue_buffer)(stream, newest) };
            return;
        };
        if !unsafe { import_slot(slot, buf, &n) } {
            unsafe { (shared.api.stream_queue_buffer)(stream, newest) };
            return;
        }
        let chunk = unsafe { &*(*buf.datas).chunk };
        let cpu = slot.cpu.as_ref().map(|map| HostCpuFrame {
            map: map.clone(),
            offset: chunk.offset as usize,
            stride: if chunk.stride > 0 {
                chunk.stride as usize
            } else {
                n.width as usize * 4
            },
            format: shm_code(n.fourcc),
        });
        slot.held = true;
        HostFrame {
            generation,
            slot: idx,
            dmabuf: slot.dmabuf.clone(),
            cpu,
            width: n.width,
            height: n.height,
            damage: unsafe { read_damage(buf, n.width, n.height) },
            stamp_ns,
        }
    };
    let count = shared.frames.fetch_add(1, Ordering::Relaxed);
    let age_ms = (arrival - stamp_ns) as f64 / 1e6;
    if count == 0 {
        eprintln!(
            "[HostCapture] output {}: first portal frame {}x{}, {} damage rect(s), {}",
            shared.sink.index,
            n.width,
            n.height,
            frame.damage.len(),
            match refusal {
                None => format!("{age_ms:.1} ms after the compositor stamped it"),
                Some(why) => format!(
                    "stamped at arrival: the compositor's stamp is {}",
                    why.says()
                ),
            }
        );
    }
    if let Some(why) = refusal.filter(|why| count > 0 && shared.stamps.lock().unwrap().first(*why))
    {
        eprintln!(
            "[HostCapture] output {}: frame {count} stamped at arrival: the compositor's stamp is {}",
            shared.sink.index,
            why.says()
        );
    }
    if trace() {
        eprintln!(
            "[HostTrace] output {} frame {count} process +{age_ms:.2}ms pts {} arrival {arrival}{}",
            shared.sink.index,
            pts.map_or("none".to_string(), |pts| pts.to_string()),
            refusal.map_or(String::new(), |why| format!(" refused: {}", why.says()))
        );
    }
    if !shared.sink.send(frame) {
        if let Some(slot) = shared.buffers.lock().unwrap().get_mut(idx) {
            slot.held = false;
        }
        unsafe { (shared.api.stream_queue_buffer)(stream, newest) };
    }
}

/// One output's stream: the consumer end of a portal monitor stream.
pub struct PwStream {
    core: Arc<PwCore>,
    stream: *mut c_void,
    shared: Arc<Shared>,
    _events: Box<PwStreamEvents>,
    _hook: Box<[u64; 8]>,
}

unsafe impl Send for PwStream {}

impl PwStream {
    /// Connect to the stream node `node_id` and start negotiating `cfg`; frames reach `sink`,
    /// cursor sprites `cursor_tx`.
    pub(crate) fn connect(
        core: &Arc<PwCore>,
        node_id: u32,
        sink: FrameSink,
        cursor_tx: std::sync::mpsc::Sender<CursorJob>,
        cfg: StreamConfig,
    ) -> Result<Self, String> {
        let api = core.api;
        let pods = enum_format_pods(&cfg);
        let shared = Arc::new(Shared {
            api,
            stream: AtomicPtr::new(ptr::null_mut()),
            sink,
            cursor_tx,
            cfg: Mutex::new(cfg),
            negotiated: Mutex::new(None),
            buffers: Mutex::new(Vec::new()),
            generation: AtomicU64::new(0),
            state: AtomicI32::new(0),
            errored: AtomicBool::new(false),
            frames: AtomicU64::new(0),
            stamps: Mutex::new(Stamps::default()),
            cursor_pos: AtomicI64::new(i64::MIN),
            cursor_hash: AtomicU64::new(0),
            params: Mutex::new(Vec::new()),
        });
        let events = Box::new(PwStreamEvents {
            state_changed: Some(on_state_changed),
            param_changed: Some(on_param_changed),
            add_buffer: Some(on_add_buffer),
            remove_buffer: Some(on_remove_buffer),
            process: Some(on_process),
            ..PwStreamEvents::empty()
        });
        let mut this = PwStream {
            core: core.clone(),
            stream: ptr::null_mut(),
            shared,
            _events: events,
            _hook: Box::new([0u64; 8]),
        };
        let result = core.locked(|| unsafe {
            let props = (api.properties_new)(ptr::null::<c_char>());
            if props.is_null() {
                return Err("pw_properties_new failed".to_string());
            }
            for (k, v) in [
                (c"media.type", c"Video"),
                (c"media.category", c"Capture"),
                (c"media.role", c"Screen"),
            ] {
                (api.properties_set)(props, k.as_ptr(), v.as_ptr());
            }
            this.stream = (api.stream_new)(core.core, c"pixelflux host capture".as_ptr(), props);
            if this.stream.is_null() {
                return Err("pw_stream_new failed".to_string());
            }
            this.shared.stream.store(this.stream, Ordering::Release);
            (api.stream_add_listener)(
                this.stream,
                this._hook.as_mut_ptr() as *mut c_void,
                &*this._events,
                Arc::as_ptr(&this.shared) as *mut c_void,
            );
            let mut ptrs: Vec<*const c_void> =
                pods.iter().map(|p| p.as_ptr() as *const c_void).collect();
            let rc = (api.stream_connect)(
                this.stream,
                PW_DIRECTION_INPUT,
                node_id,
                PW_STREAM_FLAG_AUTOCONNECT,
                ptrs.as_mut_ptr(),
                ptrs.len() as u32,
            );
            if rc < 0 {
                return Err(format!("pw_stream_connect failed ({rc})"));
            }
            Ok(())
        });
        *this.shared.params.lock().unwrap() = pods;
        result?;
        Ok(this)
    }

    /// Offer a new configuration; unchanged asks cost nothing, a change renegotiates. A monitor
    /// stream's size is its monitor's whatever the consumer prefers, so once a format stands, a
    /// new preferred size alone is kept for later offers without renegotiating: a renegotiation
    /// drops the buffers in flight, and with them the cursor bitmap KWin sends once per change.
    pub fn reconfigure(&self, cfg: StreamConfig) {
        let pods = {
            let mut cur = self.shared.cfg.lock().unwrap();
            if *cur == cfg {
                return;
            }
            let renegotiate =
                renegotiates(&cur, &cfg, self.shared.negotiated.lock().unwrap().is_some());
            *cur = cfg;
            if !renegotiate {
                return;
            }
            enum_format_pods(&cur)
        };
        self.core.locked(|| unsafe {
            let mut ptrs: Vec<*const c_void> =
                pods.iter().map(|p| p.as_ptr() as *const c_void).collect();
            (self.shared.api.stream_update_params)(
                self.stream,
                ptrs.as_mut_ptr(),
                ptrs.len() as u32,
            );
        });
        *self.shared.params.lock().unwrap() = pods;
    }

    /// Return a published frame's buffer to the compositor.
    pub fn release(&self, generation: u64, slot: usize) {
        if generation != self.shared.generation.load(Ordering::Acquire) {
            return;
        }
        self.core.locked(|| {
            let mut buffers = self.shared.buffers.lock().unwrap();
            if let Some(s) = buffers.get_mut(slot).filter(|s| s.held && !s.pw.is_null()) {
                s.held = false;
                unsafe { (self.shared.api.stream_queue_buffer)(self.stream, s.pw) };
            }
        });
    }

    /// Return every held buffer (an idled output has no consumer for them).
    pub fn release_all(&self) {
        self.core.locked(|| {
            let mut buffers = self.shared.buffers.lock().unwrap();
            for s in buffers.iter_mut().filter(|s| s.held && !s.pw.is_null()) {
                s.held = false;
                unsafe { (self.shared.api.stream_queue_buffer)(self.stream, s.pw) };
            }
        });
    }

    pub fn set_active(&self, active: bool) {
        self.core
            .locked(|| unsafe { (self.shared.api.stream_set_active)(self.stream, active) });
    }

    pub fn negotiated(&self) -> Option<Negotiated> {
        *self.shared.negotiated.lock().unwrap()
    }

    pub fn errored(&self) -> bool {
        self.shared.errored.load(Ordering::Acquire)
            || self.shared.state.load(Ordering::Acquire) == PW_STREAM_STATE_ERROR
    }

    pub fn frames(&self) -> u64 {
        self.shared.frames.load(Ordering::Relaxed)
    }

    /// The host cursor position the last cursor meta carried, in stream pixels.
    pub fn cursor_position(&self) -> Option<(i32, i32)> {
        let v = self.shared.cursor_pos.load(Ordering::Relaxed);
        (v != i64::MIN).then_some(((v >> 32) as i32, v as i32))
    }

    /// Block up to `timeout` for the stream to reach a state where frames may flow.
    pub fn wait_negotiated(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.negotiated().is_some() || self.errored() {
                return self.negotiated().is_some();
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }
}

impl Drop for PwStream {
    fn drop(&mut self) {
        self.core.locked(|| unsafe {
            self.shared.stream.store(ptr::null_mut(), Ordering::Release);
            if !self.stream.is_null() {
                (self.shared.api.stream_disconnect)(self.stream);
                (self.shared.api.stream_destroy)(self.stream);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PERIOD: i64 = 16_683_333;
    const NOW: i64 = 5_000_000_000_000;

    /// A cursor meta lands at the display's union-layout origin plus its stream position
    /// scaled to the capture size, whatever the host's own layout; without known sizes it is
    /// taken at scale one from the origin.
    #[test]
    fn a_cursor_meta_is_placed_in_the_union_layout() {
        let place = CursorPlace {
            origin: (1920, 0),
            size: (4080, 2304),
            logical: (2040, 1152),
        };
        let at = host_pointer_at(place, (4080, 2304), 3000, 1000, 7);
        assert_eq!(
            (at.x, at.y, at.scale, at.motion),
            (1920.0 + 3000.0, 1000.0, 2.0, 7)
        );
        // A stream negotiated at the logical size.
        let at = host_pointer_at(place, (2040, 1152), 1500, 500, 7);
        assert_eq!((at.x, at.y), (1920.0 + 3000.0, 1000.0));
        let at = host_pointer_at(
            CursorPlace {
                origin: (100, 200),
                ..CursorPlace::default()
            },
            (0, 0),
            10,
            20,
            0,
        );
        assert_eq!((at.x, at.y, at.scale), (110.0, 220.0, 1.0));
    }

    /// Under host capture the host pointer answers alone, None before its first cursor meta
    /// included; the compositor's share is read only without a host session.
    #[test]
    fn the_host_pointer_answers_alone_under_host_capture() {
        assert_eq!(
            prefer_host_pointer(Some(Some(1)), || panic!("share read")),
            Some(1)
        );
        assert_eq!(
            prefer_host_pointer::<u8>(Some(None), || panic!("share read")),
            None
        );
        assert_eq!(prefer_host_pointer(None, || Some(2)), Some(2));
        assert_eq!(prefer_host_pointer::<u8>(None, || None), None);
    }

    /// A cursor meta claims the moves handed over by its buffer's stamp: none before the first,
    /// the newest at or before the stamp, never one sent after it.
    #[test]
    fn a_cursor_meta_claims_the_moves_sent_before_its_stamp() {
        let sent: std::collections::VecDeque<(u64, i64)> = [(3, 100), (4, 200), (7, 300)].into();
        assert_eq!(motion_at(&sent, 99), 0);
        assert_eq!(motion_at(&sent, 100), 3);
        assert_eq!(motion_at(&sent, 250), 4);
        assert_eq!(motion_at(&sent, 1000), 7);
    }

    /// The compositor's stamp stands where it is plausible; a stamp ahead of the arrival, or
    /// older than four periods, gives way to the arrival.
    #[test]
    fn a_meta_is_stamped_by_its_buffer_where_plausible() {
        let period = 4_000_000;
        assert_eq!(meta_stamp(Some(9_000_000), 10_000_000, period), 9_000_000);
        assert_eq!(meta_stamp(Some(11_000_000), 10_000_000, period), 10_000_000);
        assert_eq!(meta_stamp(Some(1_000), 20_000_000, period), 20_000_000);
        assert_eq!(meta_stamp(None, 10_000_000, period), 10_000_000);
    }

    /// Moving a display in the union layout re-places its cursor metas without renegotiating
    /// the stream, which would drop the buffers in flight.
    #[test]
    fn a_new_place_alone_does_not_renegotiate() {
        let cur = cfg(true);
        let moved = StreamConfig {
            place: CursorPlace {
                origin: (1920, 0),
                ..cur.place
            },
            ..cur.clone()
        };
        assert!(!renegotiates(&cur, &moved, true));
    }

    /// A stamp a little before its frame's arrival, later than the last, is the capture
    /// instant; each refusal falls back to the arrival, which still advances past the last.
    #[test]
    fn a_compositor_stamp_is_taken_only_where_it_is_plausible() {
        let mut stamps = Stamps::default();
        assert_eq!(
            stamps.instant(Some(NOW - 3_000_000), NOW, PERIOD),
            (NOW - 3_000_000, None)
        );
        let next = NOW + PERIOD;
        assert_eq!(
            stamps.instant(Some(NOW - 3_000_000), next, PERIOD),
            (next, Some(StampRefusal::Repeated)),
            "a stamp reused from the previous frame"
        );
        let next = next + PERIOD;
        assert_eq!(
            stamps.instant(Some(next + 1), next, PERIOD),
            (next, Some(StampRefusal::Ahead)),
            "a stamp ahead of the arrival"
        );
        let next = next + PERIOD;
        assert_eq!(
            stamps.instant(Some(next - 5 * PERIOD), next, PERIOD),
            (next, Some(StampRefusal::Stale)),
            "a stamp five frame periods old"
        );
        let next = next + PERIOD;
        assert_eq!(
            stamps.instant(Some(1_759_000_000_000_000_000), next, PERIOD),
            (next, Some(StampRefusal::Ahead)),
            "a CLOCK_REALTIME stamp"
        );
        let next = next + PERIOD;
        assert_eq!(
            stamps.instant(Some(40_000_000), next, PERIOD),
            (next, Some(StampRefusal::Stale)),
            "a stamp counted from the stream's start"
        );
        let next = next + PERIOD;
        assert_eq!(
            stamps.instant(None, next, PERIOD),
            (next, Some(StampRefusal::Missing)),
            "no stamp (the header meta absent, or zero)"
        );
        let next = next + PERIOD;
        assert_eq!(
            stamps.instant(Some(next - 2_000_000), next, PERIOD),
            (next - 2_000_000, None)
        );
    }

    /// A stamp that is plausible on its own but earlier than an arrival given to the frame
    /// before is refused too, so the instants never go back.
    #[test]
    fn the_instants_never_go_back() {
        let mut stamps = Stamps::default();
        let (first, why) = stamps.instant(None, NOW, PERIOD);
        assert_eq!((first, why), (NOW, Some(StampRefusal::Missing)));
        let (second, why) = stamps.instant(Some(NOW - 1_000_000), NOW + 1_000_000, PERIOD);
        assert_eq!(why, Some(StampRefusal::Repeated));
        assert!(second > first);
        let mut seen = Stamps::default();
        assert!(seen.first(StampRefusal::Repeated));
        assert!(!seen.first(StampRefusal::Repeated));
        assert!(seen.first(StampRefusal::Stale));
    }

    fn cfg(zero_copy: bool) -> StreamConfig {
        StreamConfig {
            zero_copy,
            dma_formats: vec![
                (FOURCC_XR24, vec![0x0300_0000_0000_0001, 0]),
                (FOURCC_AR24, vec![0]),
            ],
            fps_milli: 59_940,
            expect: (1920, 1080),
            place: CursorPlace {
                origin: (0, 0),
                size: (1920, 1080),
                logical: (1920, 1080),
            },
        }
    }

    /// A zero-copy ask lists one modifier-bearing entry per importable fourcc ahead of the
    /// plain entries, each holding the rate cap and the expected size; a CPU ask lists only
    /// the plain entries.
    #[test]
    fn enum_formats_lead_with_modifiers_when_zero_copy() {
        let pods = enum_format_pods(&cfg(true));
        assert_eq!(pods.len(), 2 + SHM_FORMATS.len());
        let (_, _, first) = object_props(&pods[0]).unwrap();
        let modifier = find_prop(&first, SPA_FORMAT_VIDEO_MODIFIER).unwrap();
        assert_eq!(
            modifier.flags,
            SPA_POD_PROP_FLAG_MANDATORY | SPA_POD_PROP_FLAG_DONT_FIXATE
        );
        assert_eq!(modifier.as_i64(), Some(0x0300_0000_0000_0001));
        assert_eq!(
            find_prop(&first, SPA_FORMAT_VIDEO_FORMAT).unwrap().as_u32(),
            Some(SPA_VIDEO_FORMAT_BGRX)
        );
        assert_eq!(
            find_prop(&first, SPA_FORMAT_VIDEO_SIZE).unwrap().as_pair(),
            Some((1920, 1080))
        );
        assert_eq!(
            find_prop(&first, SPA_FORMAT_VIDEO_MAX_FRAMERATE)
                .unwrap()
                .as_pair(),
            Some((59_940, 1000))
        );
        let (_, _, plain) = object_props(&pods[2]).unwrap();
        assert!(find_prop(&plain, SPA_FORMAT_VIDEO_MODIFIER).is_none());
        let cpu = enum_format_pods(&cfg(false));
        assert_eq!(cpu.len(), SHM_FORMATS.len());
        for pod in &cpu {
            let (_, _, props) = object_props(pod).unwrap();
            assert!(find_prop(&props, SPA_FORMAT_VIDEO_MODIFIER).is_none());
        }
    }

    /// A fixated format the compositor sends back parses into geometry, fourcc, and the
    /// dmabuf decision, and the buffer params answer with the matching data type.
    #[test]
    fn fixated_format_parses_and_selects_buffer_type() {
        let fixated = object(SPA_TYPE_OBJECT_FORMAT, SPA_PARAM_FORMAT, |p| {
            prop(p, SPA_FORMAT_MEDIA_TYPE, |v| {
                pod_id(v, SPA_MEDIA_TYPE_VIDEO)
            });
            prop(p, SPA_FORMAT_MEDIA_SUBTYPE, |v| {
                pod_id(v, SPA_MEDIA_SUBTYPE_RAW)
            });
            prop(p, SPA_FORMAT_VIDEO_FORMAT, |v| {
                pod_id(v, SPA_VIDEO_FORMAT_BGRA)
            });
            prop(p, SPA_FORMAT_VIDEO_MODIFIER, |v| {
                pod_choice_long(v, SPA_CHOICE_NONE, &[0x0300_0000_0000_0007])
            });
            prop(p, SPA_FORMAT_VIDEO_SIZE, |v| pod_rect(v, 1280, 720));
            prop(p, SPA_FORMAT_VIDEO_FRAMERATE, |v| pod_frac(v, 0, 1));
        });
        let n = parse_format(&fixated).unwrap();
        assert_eq!(
            n,
            Negotiated {
                width: 1280,
                height: 720,
                fourcc: FOURCC_AR24,
                modifier: Some(0x0300_0000_0000_0007)
            }
        );
        let params = negotiated_params(&n, true);
        let (_, _, buffers) = object_props(&params[0]).unwrap();
        assert_eq!(
            find_prop(&buffers, SPA_PARAM_BUFFERS_DATATYPE)
                .unwrap()
                .as_u32(),
            Some(1 << SPA_DATA_DMABUF)
        );
        assert_eq!(
            find_prop(&buffers, SPA_PARAM_BUFFERS_BUFFERS)
                .unwrap()
                .as_u32(),
            Some(4)
        );
        let memfd = object(SPA_TYPE_OBJECT_FORMAT, SPA_PARAM_FORMAT, |p| {
            prop(p, SPA_FORMAT_VIDEO_FORMAT, |v| {
                pod_id(v, SPA_VIDEO_FORMAT_RGBX)
            });
            prop(p, SPA_FORMAT_VIDEO_SIZE, |v| pod_rect(v, 640, 480));
        });
        let n = parse_format(&memfd).unwrap();
        assert_eq!((n.fourcc, n.modifier), (FOURCC_XB24, None));
        let params = negotiated_params(&n, false);
        let (_, _, buffers) = object_props(&params[0]).unwrap();
        assert_eq!(
            find_prop(&buffers, SPA_PARAM_BUFFERS_DATATYPE)
                .unwrap()
                .as_u32(),
            Some(1 << SPA_DATA_MEMFD)
        );
        assert_eq!(shm_code(FOURCC_XR24), 1);
        assert_eq!(shm_code(FOURCC_AR24), 0);
        assert_eq!(shm_code(FOURCC_XB24), FOURCC_XB24);
    }

    /// A new preferred size alone keeps a negotiated format, but renegotiates before one stands;
    /// a new rate cap or buffer type always renegotiates.
    #[test]
    fn a_preferred_size_alone_keeps_a_negotiated_format() {
        let cur = cfg(false);
        let resized = StreamConfig {
            expect: (1280, 720),
            ..cur.clone()
        };
        assert!(!renegotiates(&cur, &resized, true));
        assert!(renegotiates(&cur, &resized, false));
        let capped = StreamConfig {
            fps_milli: 30_000,
            ..resized.clone()
        };
        assert!(renegotiates(&cur, &capped, true));
        assert!(renegotiates(&cur, &cfg(true), true));
    }

    /// The cursor meta's size range takes the fixed sizes compositors offer it at, Mutter's
    /// 384x384 and KWin's 256x256; PipeWire leaves off a meta whose offered size falls outside.
    #[test]
    fn cursor_meta_range_takes_the_compositors_sizes() {
        let n = Negotiated {
            width: 1280,
            height: 720,
            fourcc: FOURCC_XR24,
            modifier: None,
        };
        let range = negotiated_params(&n, false)
            .iter()
            .find_map(|pod| {
                let (_, _, props) = object_props(pod)?;
                if find_prop(&props, SPA_PARAM_META_TYPE)?.as_u32()? != SPA_META_CURSOR {
                    return None;
                }
                let size = find_prop(&props, SPA_PARAM_META_SIZE)?;
                let at =
                    |off: usize| u32::from_ne_bytes(size.payload[off..off + 4].try_into().unwrap());
                assert_eq!((size.ty, at(0)), (SPA_TYPE_CHOICE, SPA_CHOICE_RANGE));
                // Default, minimum, maximum, after the choice and child headers.
                Some((at(20), at(24)))
            })
            .unwrap();
        for edge in [256, 384] {
            let size = cursor_meta_size(edge, edge);
            assert!(
                range.0 <= size && size <= range.1,
                "{edge}x{edge} outside {range:?}"
            );
        }
    }
}
