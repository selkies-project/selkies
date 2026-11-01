//! Host-capture mode: pixelflux as a CLIENT of an external Wayland compositor
//! (e.g. labwc running `WLR_BACKENDS=headless`), inverting the nested topology.
//!
//! The host compositor owns the session — one seat, one selection, one screen
//! model — and pixelflux captures and injects as a privileged-protocol client:
//! frames via `ext_image_copy_capture_v1` where the host offers it (wlroots
//! 0.19+, KWin 6.2+, cosmic) and `zwlr_screencopy_v1` (v3 `copy_with_damage`)
//! everywhere else — both damage-gated, so idle screens cost nothing
//! (`PIXELFLUX_HOST_CAPTURE=zwlr` forces the fallback); input over libei where
//! the backend grants an EIS socket, else a persistent `zwp_virtual_keyboard_v1`
//! device carrying selkies' own keymap text with `zwlr_virtual_pointer_v1`, else
//! kernel uinput devices where `/dev/uinput` is writable. Zero-copy is preserved by
//! allocating the capture buffers from pixelflux's OWN GBM device (render node —
//! no privileges): the compositor blits straight into the dmabufs the encoder
//! imports, so no CPU ever touches a frame. A host that cannot import our
//! dmabufs (other GPU, software renderer) and any run without a GPU degrade to
//! wl_shm buffers feeding the CPU encode pool.
//!
//! Every host `wl_output` maps to one selkies display id, in registry order: a
//! control thread on the primary connection owns wlr-output-management and
//! applies the whole layout (per-output custom modes plus row positions) as one
//! atomic configuration, while each output gets its own capture connection and
//! thread. Input proxies are called from the calloop thread directly
//! (wayland-client proxies are thread-safe); pointer coordinates arrive in the
//! same union-layout space selkies uses, so the virtual-pointer extent is the
//! layout's bounding box. All capture-side waits poll a wake pipe next to the
//! Wayland socket, so a resize or teardown lands immediately even while the
//! thread is parked in a `copy_with_damage` wait on a static screen.
//!
//! A host without those protocols (GNOME, KDE before ext-image-copy-capture) is captured
//! through xdg-desktop-portal instead: one RemoteDesktop session hands out a PipeWire stream per
//! monitor and takes the input the seat lacks a virtual device for (`portal`, `pwcapture`).
//! Each rung is chosen per capability, ordered by measured delivery cost: a socket write to the
//! compositor (libei, then the virtual-input protocols) runs about 4 us, a uinput write reaching
//! evdev about 12 us, and a portal `Notify*` D-Bus call about 2 ms, so the kernel serves as the
//! universal fallback rather than the first choice, and the portal comes last. A uinput pointer
//! declares a normalized absolute axis, since the layout extent changes while a device's range
//! cannot. Frames take the Wayland protocol where the host offers it and the portal where it does
//! not — so a
//! KWin that serves ext-image-copy-capture but no virtual pointer still gets its pointer from
//! the portal. Portal keys go by keysym, resolved from selkies' own keymap, so the host applies
//! its layout to the symbol rather than to a keycode from another layout. The compositor owns
//! the portal buffers: a dmabuf frame is imported where it lies and a memfd frame is read in
//! place, and the portal stream is asked for at most the capture's rate so no frame is
//! produced only to be dropped.

use std::fs::File;
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use gbm::{Device as GbmDevice, Format as GbmFormat};
use smithay::backend::allocator::Buffer as _;
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::input::keyboard::xkb;
use smithay::utils::{Physical, Rectangle};
use wayland_client::protocol::{wl_output, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_image_capture_source_v1::ExtImageCaptureSourceV1,
    ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1},
    ext_image_copy_capture_manager_v1::{self, ExtImageCopyCaptureManagerV1},
    ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
    zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use wayland_protocols_wlr::output_management::v1::client::{
    zwlr_output_configuration_head_v1::ZwlrOutputConfigurationHeadV1,
    zwlr_output_configuration_v1::{self, ZwlrOutputConfigurationV1},
    zwlr_output_head_v1::{self, ZwlrOutputHeadV1},
    zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
    zwlr_output_mode_v1::ZwlrOutputModeV1,
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

use crate::wayland::cursor::CursorJob;
use crate::wayland::eiclient::EiInjector;
use crate::wayland::portal::{self, PortalSession, PortalStream};
use crate::wayland::pwcapture::{PwCore, PwStream, StreamConfig};
use crate::wayland::wlclient::{
    SyncState, bounded_roundtrip, drain_pipe, impl_sync_callback, memfd_with, socket_path,
    wait_readable2, wake_pipe, wake_write,
};

const KEYMAP_FORMAT_XKB_V1: u32 = 1;
/// Frames in flight between a capture thread and the encode tick. Two slots:
/// one being blitted by the compositor while the previous one is encoded.
const SLOTS: usize = 2;

/// A frame the compositor finished blitting, ready for the encoder.
pub struct HostFrame {
    pub(crate) generation: u64,
    pub(crate) slot: usize,
    /// GPU path: the filled dmabuf (Arc-backed; the buffer itself stays owned by
    /// the capture thread and is reused once the slot is released).
    pub dmabuf: Option<Dmabuf>,
    /// Software path: the shm mapping itself — the consumer converts straight
    /// out of it (one copy total); the slot is not reused until release.
    pub cpu: Option<HostCpuFrame>,
    pub width: i32,
    pub height: i32,
    pub damage: Vec<Rectangle<i32, Physical>>,
    /// The compositor's CLOCK_MONOTONIC stamp for the frame, or its arrival when the host gives none.
    pub stamp_ns: i64,
}

/// CLOCK_MONOTONIC in nanoseconds, the clock PipeWire stamps frames with.
pub(crate) fn now_ns() -> i64 {
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut t) };
    t.tv_sec * 1_000_000_000 + t.tv_nsec
}

/// `PIXELFLUX_HOST_TRACE` set: log every host frame's age on arrival and once encoded.
pub(crate) fn trace() -> bool {
    static ON: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| std::env::var_os("PIXELFLUX_HOST_TRACE").is_some());
    *ON
}

/// Borrow-by-Arc view of a software frame still sitting in its shm mapping.
pub struct HostCpuFrame {
    pub(crate) map: Arc<dyn AsRef<[u8]> + Send + Sync>,
    /// First byte of the frame within the mapping.
    pub(crate) offset: usize,
    pub(crate) stride: usize,
    pub(crate) format: u32,
}

/// Source bytes-per-pixel and whether R and B swap to reach BGRA, for one announced wl_shm
/// format. The consumers downstream all read BGRA (byte order B,G,R,A), so a format is mapped by
/// where its red and blue land in memory under the little-endian byte order wayland.xml specifies:
///
/// - `Xrgb8888` / `Argb8888` (`[31:0] x:R:G:B` LE) are B,G,R,x in memory: already BGRA, no swap.
/// - `Xbgr8888` / `Abgr8888` (`[31:0] x:B:G:R` LE) are R,G,B,x in memory: swap R/B.
/// - `Bgr888` (`[23:0] B:G:R` LE) is R,G,B in memory: swap R/B (the 24-bit NVIDIA-GLES read path).
/// - `Rgb888` (`[23:0] R:G:B` LE) is B,G,R in memory: already BGR order, no swap.
///
/// The empirical NVIDIA-GLES host could not be exercised here, so `Bgr888` follows the wl_shm
/// definition literally: `[23:0] B:G:R little endian` places R in the low byte, so byte 0 is red and
/// reaching BGRA swaps R and B — the opposite of a straight copy.
fn shm_src_layout(format: u32) -> (usize, bool) {
    match format {
        f if f == wl_shm::Format::Xbgr8888 as u32 || f == wl_shm::Format::Abgr8888 as u32 => {
            (4, true)
        }
        f if f == wl_shm::Format::Bgr888 as u32 => (3, true),
        f if f == wl_shm::Format::Rgb888 as u32 => (3, false),
        _ => (4, false),
    }
}

/// Convert one source row to a BGRA destination row per `(src_bpp, swap_rb)` from
/// `shm_src_layout`: a 4-byte source either copies straight or swaps R/B keeping alpha, a 3-byte
/// source fills opaque alpha and swaps R/B only when the layout calls for it.
fn convert_shm_row(src: &[u8], dst: &mut [u8], src_bpp: usize, swap_rb: bool) {
    match (src_bpp, swap_rb) {
        (4, false) => {
            let n = dst.len().min(src.len());
            dst[..n].copy_from_slice(&src[..n]);
        }
        (4, true) => {
            for (d, s) in dst
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(src.as_chunks::<4>().0)
            {
                d[0] = s[2];
                d[1] = s[1];
                d[2] = s[0];
                d[3] = s[3];
            }
        }
        // One 4-byte store per pixel rather than four byte stores: measured ~1.7x on a
        // 3-byte source. The 4-byte arms above vectorize better as they stand.
        (_, true) => {
            for (d, s) in dst
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(src.as_chunks::<3>().0)
            {
                d.copy_from_slice(&[s[2], s[1], s[0], 0xff]);
            }
        }
        (_, false) => {
            for (d, s) in dst
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(src.as_chunks::<3>().0)
            {
                d.copy_from_slice(&[s[0], s[1], s[2], 0xff]);
            }
        }
    }
}

impl HostCpuFrame {
    /// Convert the frame into tight BGRA rows in `dst` (sized `w*4*h`); the announced shm
    /// format decides the per-pixel conversion (compositors pick their renderer's preferred read
    /// format, e.g. 24-bit BGR on NVIDIA GLES).
    pub fn write_bgra(&self, w: i32, h: i32, dst: &mut [u8]) {
        let row = (w * 4) as usize;
        let (src_bpp, swap_rb) = shm_src_layout(self.format);
        let bytes: &[u8] = (*self.map).as_ref();
        let bytes = &bytes[self.offset.min(bytes.len())..];
        for y in 0..h as usize {
            let src_start = y * self.stride;
            let src_end = (src_start + src_bpp * w as usize).min(bytes.len());
            if src_start >= src_end || (y + 1) * row > dst.len() {
                break;
            }
            let src_row = &bytes[src_start..src_end];
            let dst_row = &mut dst[y * row..(y + 1) * row];
            convert_shm_row(src_row, dst_row, src_bpp, swap_rb);
        }
    }
}

/// One display's slice of the union layout, as selkies configured it. Slots are
/// keyed by selkies display id ('display2' is id 2 — ids are sparse); active ids
/// map onto host outputs by ascending rank.
#[derive(Clone, Copy, Default)]
struct LayoutSlot {
    want: (i32, i32),
    pos: (i32, i32),
    active: bool,
    /// The consumer imports dmabufs directly, so the compositor blits into GPU
    /// buffers. A CPU encode path clears this and takes shm frames instead.
    zero_copy: bool,
    /// The host paints its cursor into the frames (the consumer's native cursor
    /// rendering); off, the consumer draws the cursor itself from the sprite callback.
    paint_cursor: bool,
    /// The capture's rate in millihertz, the most a portal stream is asked to deliver.
    fps_milli: u32,
}

/// What a capture thread is currently aiming at: the encoder's dimensions, the buffer
/// type its consumer can take, and whether the host paints the cursor in. A size or
/// buffer-type change forces a slot renegotiation; a cursor change reopens the ext
/// session, whose option is fixed at creation.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Want {
    size: (i32, i32),
    zero_copy: bool,
    paint_cursor: bool,
    fps_milli: u32,
}

enum ToHost {
    Start {
        width: i32,
        height: i32,
        zero_copy: bool,
        paint_cursor: bool,
        fps_milli: u32,
    },
    /// Slot return; `generation` guards against slots recycled by a renegotiation
    /// while the consumer still held the frame.
    Release { generation: u64, slot: usize },
    /// Park the capture (display stopped) without ending the thread.
    Idle,
}

enum CtrlMsg {
    Apply { epoch: u64, slots: Vec<LayoutSlot> },
}

/// The capture thread's end of one output's frame queue: the frames, an exact count of those
/// queued and not yet taken (so a wake that the timer has already served is recognized as
/// such), and the calloop wake that follows every frame.
#[derive(Clone)]
pub(crate) struct FrameSink {
    pub(crate) tx: Sender<HostFrame>,
    pub(crate) queued: Arc<AtomicI64>,
    pub(crate) wake: smithay::reexports::calloop::channel::Sender<usize>,
    pub(crate) index: usize,
}

impl FrameSink {
    /// Queue `frame` and wake the consumer; false once the consumer is gone.
    pub(crate) fn send(&self, frame: HostFrame) -> bool {
        if self.tx.send(frame).is_err() {
            return false;
        }
        self.queued.fetch_add(1, Ordering::AcqRel);
        let _ = self.wake.send(self.index);
        true
    }
}

/// Outcome ledger for layout requests: every Apply carries an epoch, and the
/// control thread records whether the newest one it processed was realized by
/// the host. A request is answered once `decided` has caught up to its epoch;
/// a newer request subsumes an older one (the layout map is cumulative), so
/// the newest verdict stands for every epoch at or below it. Polled, never
/// waited on: the session side reads it from its own loop.
#[derive(Default)]
struct LayoutLedger {
    epoch: u64,
    decided: u64,
    realized: bool,
}

impl LayoutLedger {
    /// Number the next request.
    fn issue(&mut self) -> u64 {
        self.epoch += 1;
        self.epoch
    }

    /// Record the host's verdict on `epoch` (ignored if a newer one is already in).
    fn decide(&mut self, epoch: u64, realized: bool) {
        if epoch > self.decided {
            self.decided = epoch;
            self.realized = realized;
        }
    }

    /// The verdict covering `epoch`: `None` while the host has not answered it yet.
    fn outcome(&self, epoch: u64) -> Option<bool> {
        (self.decided >= epoch).then_some(self.realized)
    }
}

/// Bound for one layout application on the control thread: a host that has not answered by
/// then is settled without it, and the session follows whatever mode the host announces.
pub const LAYOUT_DEADLINE: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Control connection: seat + virtual input devices + wlr-output-management.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct CtrlState {
    seat: Option<wl_seat::WlSeat>,
    vk_mgr: Option<ZwpVirtualKeyboardManagerV1>,
    vptr_mgr: Option<ZwlrVirtualPointerManagerV1>,
    has_screencopy: bool,
    has_ext_capture: bool,
    has_ext_source: bool,
    outputs: Vec<(wl_output::WlOutput, Option<String>)>,
    /// Current mode per output, registry-indexed and shared with the session:
    /// when the host keeps its own size (refused or unmanageable layout), the
    /// capture follows this instead of gating on the size it asked for.
    sizes: Arc<Mutex<Vec<(i32, i32)>>>,
    /// Layout position and integer scale per output, registry-indexed.
    positions: Vec<(i32, i32)>,
    scales: Vec<i32>,
    /// Registry indices in display order (natural name sort — registry
    /// announcement order is not guaranteed to match output creation order).
    order: Vec<usize>,
    output_mgr: Option<ZwlrOutputManagerV1>,
    heads: Vec<(ZwlrOutputHeadV1, Option<String>)>,
    om_serial: Option<u32>,
    cfg_result: Option<bool>,
    cfg_canceled: bool,
    sync_done: bool,
}

/// Natural sort key for an output name: text prefix plus trailing number, so
/// HEADLESS-2 orders before HEADLESS-10. Unnamed outputs keep registry order
/// after every named one.
fn output_order_key(name: Option<&String>, registry_idx: usize) -> (bool, String, u64, usize) {
    match name {
        Some(n) => {
            let digits_at = n
                .rfind(|c: char| !c.is_ascii_digit())
                .map(|i| i + 1)
                .unwrap_or(0);
            let num = n[digits_at..].parse::<u64>().unwrap_or(0);
            (false, n[..digits_at].to_string(), num, registry_idx)
        }
        None => (true, String::new(), 0, registry_idx),
    }
}

impl SyncState for CtrlState {
    fn sync_done_mut(&mut self) -> &mut bool {
        &mut self.sync_done
    }
}
impl_sync_callback!(CtrlState);

impl Dispatch<wl_registry::WlRegistry, ()> for CtrlState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_seat" if state.seat.is_none() => {
                    state.seat = Some(registry.bind(name, 1, qh, ()))
                }
                "zwp_virtual_keyboard_manager_v1" => {
                    state.vk_mgr = Some(registry.bind(name, 1, qh, ()))
                }
                "zwlr_virtual_pointer_manager_v1" => {
                    state.vptr_mgr = Some(registry.bind(name, version.min(2), qh, ()))
                }
                "zwlr_screencopy_manager_v1" if version >= 3 => state.has_screencopy = true,
                "ext_image_copy_capture_manager_v1" => state.has_ext_capture = true,
                "ext_output_image_capture_source_manager_v1" => state.has_ext_source = true,
                "wl_output" => {
                    let idx = state.outputs.len();
                    let out = registry.bind(name, version.min(4), qh, idx);
                    state.outputs.push((out, None));
                }
                "zwlr_output_manager_v1" => state.output_mgr = Some(registry.bind(name, 1, qh, ())),
                _ => {}
            }
        }
    }
}

macro_rules! impl_output_name {
    ($t:ty) => {
        impl Dispatch<wl_output::WlOutput, usize> for $t {
            fn event(
                state: &mut Self,
                _: &wl_output::WlOutput,
                event: wl_output::Event,
                idx: &usize,
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
                if let wl_output::Event::Name { name } = event {
                    if let Some(o) = state.outputs.get_mut(*idx) {
                        o.1 = Some(name);
                    }
                }
            }
        }
    };
}
impl Dispatch<wl_output::WlOutput, usize> for CtrlState {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        idx: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_output::Event::Name { name } => {
                if let Some(o) = state.outputs.get_mut(*idx) {
                    o.1 = Some(name);
                }
            }
            wl_output::Event::Mode {
                flags,
                width,
                height,
                ..
            } if flags
                .into_result()
                .is_ok_and(|f| f.contains(wl_output::Mode::Current)) =>
            {
                let mut sizes = state.sizes.lock().unwrap();
                if sizes.len() <= *idx {
                    sizes.resize(*idx + 1, (0, 0));
                }
                sizes[*idx] = (width, height);
            }
            wl_output::Event::Geometry { x, y, .. } => {
                if state.positions.len() <= *idx {
                    state.positions.resize(*idx + 1, (0, 0));
                }
                state.positions[*idx] = (x, y);
            }
            wl_output::Event::Scale { factor } => {
                if state.scales.len() <= *idx {
                    state.scales.resize(*idx + 1, 1);
                }
                state.scales[*idx] = factor.max(1);
            }
            _ => {}
        }
    }
}

delegate_noop!(CtrlState: ignore wl_seat::WlSeat);
delegate_noop!(CtrlState: ZwpVirtualKeyboardManagerV1);
delegate_noop!(CtrlState: ZwpVirtualKeyboardV1);
delegate_noop!(CtrlState: ZwlrVirtualPointerManagerV1);
delegate_noop!(CtrlState: ZwlrVirtualPointerV1);
delegate_noop!(CtrlState: ignore ZwlrOutputModeV1);
delegate_noop!(CtrlState: ZwlrOutputConfigurationHeadV1);

impl Dispatch<ZwlrOutputManagerV1, ()> for CtrlState {
    fn event(
        state: &mut Self,
        _: &ZwlrOutputManagerV1,
        event: zwlr_output_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_output_manager_v1::Event::Head { head } => state.heads.push((head, None)),
            zwlr_output_manager_v1::Event::Done { serial } => state.om_serial = Some(serial),
            _ => {}
        }
    }

    wayland_client::event_created_child!(CtrlState, ZwlrOutputManagerV1, [
        zwlr_output_manager_v1::EVT_HEAD_OPCODE => (ZwlrOutputHeadV1, ()),
    ]);
}

impl Dispatch<ZwlrOutputHeadV1, ()> for CtrlState {
    fn event(
        state: &mut Self,
        head: &ZwlrOutputHeadV1,
        event: zwlr_output_head_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_output_head_v1::Event::Name { name } => {
                if let Some(h) = state.heads.iter_mut().find(|(h, _)| h.id() == head.id()) {
                    h.1 = Some(name);
                }
            }
            zwlr_output_head_v1::Event::Finished => {
                state.heads.retain(|(h, _)| h.id() != head.id());
            }
            _ => {}
        }
    }

    wayland_client::event_created_child!(CtrlState, ZwlrOutputHeadV1, [
        zwlr_output_head_v1::EVT_MODE_OPCODE => (ZwlrOutputModeV1, ()),
    ]);
}

impl Dispatch<ZwlrOutputConfigurationV1, ()> for CtrlState {
    fn event(
        state: &mut Self,
        _: &ZwlrOutputConfigurationV1,
        event: zwlr_output_configuration_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_output_configuration_v1::Event::Succeeded => state.cfg_result = Some(true),
            zwlr_output_configuration_v1::Event::Failed => state.cfg_result = Some(false),
            zwlr_output_configuration_v1::Event::Cancelled => state.cfg_canceled = true,
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Capture connections: one per host output (screencopy + buffer allocation).
// ---------------------------------------------------------------------------

#[derive(Default)]
struct CaptureState {
    shm: Option<wl_shm::WlShm>,
    dmabuf: Option<ZwpLinuxDmabufV1>,
    screencopy: Option<ZwlrScreencopyManagerV1>,
    ext_capture: Option<ExtImageCopyCaptureManagerV1>,
    ext_source_mgr: Option<ExtOutputImageCaptureSourceManagerV1>,
    outputs: Vec<(wl_output::WlOutput, Option<String>)>,
    // Per-frame capture negotiation/results: the buffer types screencopy offered, as
    // (fourcc, width, height) for dmabuf and (format, width, height, stride) for shm.
    announce_dmabuf: Option<(u32, i32, i32)>,
    announce_shm: Option<(u32, i32, i32, i32)>,
    buffer_done: bool,
    damage: Vec<Rectangle<i32, Physical>>,
    ready: bool,
    failed: bool,
    sync_done: bool,
    // Non-immed zwp_linux_buffer_params result: the host either delivered a
    // wl_buffer for our dmabuf or refused the import.
    params_created: Option<wayland_client::protocol::wl_buffer::WlBuffer>,
    params_failed: bool,
    // ext session constraints stream in flight; `done` seals it into the committed
    // set and bumps the serial (a new set arrives whenever the output changes).
    ext_pending_size: Option<(i32, i32)>,
    ext_pending_shm: Vec<u32>,
    ext_pending_dma: Vec<(u32, Vec<u64>)>,
    ext_size: Option<(i32, i32)>,
    ext_shm_formats: Vec<u32>,
    ext_dma_formats: Vec<(u32, Vec<u64>)>,
    ext_serial: u64,
    ext_stopped: bool,
    ext_fail_reason: Option<ext_image_copy_capture_frame_v1::FailureReason>,
}

impl CaptureState {
    fn reset_frame(&mut self) {
        self.announce_dmabuf = None;
        self.announce_shm = None;
        self.buffer_done = false;
        self.damage.clear();
        self.ready = false;
        self.failed = false;
        self.ext_fail_reason = None;
    }
}

impl SyncState for CaptureState {
    fn sync_done_mut(&mut self) -> &mut bool {
        &mut self.sync_done
    }
}
impl_sync_callback!(CaptureState);
impl_output_name!(CaptureState);

impl Dispatch<wl_registry::WlRegistry, ()> for CaptureState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
                "zwp_linux_dmabuf_v1" if version >= 3 => {
                    state.dmabuf = Some(registry.bind(name, 3, qh, ()))
                }
                "zwlr_screencopy_manager_v1" if version >= 3 => {
                    state.screencopy = Some(registry.bind(name, 3, qh, ()))
                }
                "ext_image_copy_capture_manager_v1" => {
                    state.ext_capture = Some(registry.bind(name, 1, qh, ()))
                }
                "ext_output_image_capture_source_manager_v1" => {
                    state.ext_source_mgr = Some(registry.bind(name, 1, qh, ()))
                }
                "wl_output" => {
                    let idx = state.outputs.len();
                    let out = registry.bind(name, version.min(4), qh, idx);
                    state.outputs.push((out, None));
                }
                _ => {}
            }
        }
    }
}

delegate_noop!(CaptureState: ignore wl_shm::WlShm);
delegate_noop!(CaptureState: ignore wl_shm_pool::WlShmPool);
delegate_noop!(CaptureState: ignore wayland_client::protocol::wl_buffer::WlBuffer);
delegate_noop!(CaptureState: ZwlrScreencopyManagerV1);
delegate_noop!(CaptureState: ExtImageCopyCaptureManagerV1);
delegate_noop!(CaptureState: ExtOutputImageCaptureSourceManagerV1);
delegate_noop!(CaptureState: ExtImageCaptureSourceV1);

impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for CaptureState {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_session_v1::Event;
        match event {
            Event::BufferSize { width, height } => {
                state.ext_pending_size = Some((width as i32, height as i32));
            }
            Event::ShmFormat { format } => {
                let raw = match format {
                    WEnum::Value(f) => f as u32,
                    WEnum::Unknown(u) => u,
                };
                state.ext_pending_shm.push(raw);
            }
            Event::DmabufDevice { .. } => {
                // Buffers are allocated from pixelflux's own render node, like the
                // wlr-screencopy path; a cross-device host fails the dmabuf import
                // and the shm formats below still apply.
            }
            Event::DmabufFormat { format, modifiers } => {
                let mods = modifiers
                    .as_chunks::<8>()
                    .0
                    .iter()
                    .map(|c| u64::from_ne_bytes(*c))
                    .collect();
                state.ext_pending_dma.push((format, mods));
            }
            Event::Done => {
                state.ext_size = state.ext_pending_size.take();
                state.ext_shm_formats = std::mem::take(&mut state.ext_pending_shm);
                state.ext_dma_formats = std::mem::take(&mut state.ext_pending_dma);
                state.ext_serial += 1;
            }
            Event::Stopped => state.ext_stopped = true,
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for CaptureState {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_frame_v1::Event;
        match event {
            Event::Damage {
                x,
                y,
                width,
                height,
            } => {
                state
                    .damage
                    .push(Rectangle::new((x, y).into(), (width, height).into()));
            }
            Event::Ready => state.ready = true,
            Event::Failed { reason } => {
                state.failed = true;
                if let WEnum::Value(r) = reason {
                    state.ext_fail_reason = Some(r);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpLinuxDmabufV1, ()> for CaptureState {
    fn event(
        _: &mut Self,
        _: &ZwpLinuxDmabufV1,
        _: <ZwpLinuxDmabufV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // format/modifier advertisements: the allocation follows the screencopy
        // frame's announcement instead.
    }
}

impl Dispatch<ZwpLinuxBufferParamsV1, ()> for CaptureState {
    fn event(
        state: &mut Self,
        _: &ZwpLinuxBufferParamsV1,
        event: zwp_linux_buffer_params_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_linux_buffer_params_v1::Event::Created { buffer } => {
                state.params_created = Some(buffer);
            }
            zwp_linux_buffer_params_v1::Event::Failed => state.params_failed = true,
            _ => {}
        }
    }

    fn event_created_child(
        _opcode: u16,
        qhandle: &QueueHandle<Self>,
    ) -> std::sync::Arc<dyn wayland_client::backend::ObjectData> {
        qhandle.make_data::<wayland_client::protocol::wl_buffer::WlBuffer, _>(())
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for CaptureState {
    fn event(
        state: &mut Self,
        _: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format: WEnum::Value(f),
                width,
                height,
                stride,
            } => {
                state.announce_shm = Some((f as u32, width as i32, height as i32, stride as i32));
            }
            zwlr_screencopy_frame_v1::Event::LinuxDmabuf {
                format,
                width,
                height,
            } => {
                state.announce_dmabuf = Some((format, width as i32, height as i32));
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => state.buffer_done = true,
            zwlr_screencopy_frame_v1::Event::Damage {
                x,
                y,
                width,
                height,
            } => {
                state.damage.push(Rectangle::new(
                    (x as i32, y as i32).into(),
                    (width as i32, height as i32).into(),
                ));
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => state.ready = true,
            zwlr_screencopy_frame_v1::Event::Failed => state.failed = true,
            _ => {}
        }
    }
}

enum SlotBuffer {
    Gpu {
        _bo: gbm::BufferObject<()>,
        dmabuf: Dmabuf,
        wl: wayland_client::protocol::wl_buffer::WlBuffer,
    },
    Cpu {
        _pool: wl_shm_pool::WlShmPool,
        map: Arc<memmap2::MmapMut>,
        stride: i32,
        format: u32,
        wl: wayland_client::protocol::wl_buffer::WlBuffer,
    },
}

/// Release the host proxies a slot created. Dropping a wayland-client proxy sends no request,
/// so without this every resize or buffer-type switch strands the slot's `wl_buffer` — and, on the
/// shm path, its `wl_shm_pool` — in the host compositor for the connection's lifetime. The buffer is
/// destroyed before the pool it was created from.
impl Drop for SlotBuffer {
    fn drop(&mut self) {
        match self {
            SlotBuffer::Gpu { wl, .. } => wl.destroy(),
            SlotBuffer::Cpu { _pool, wl, .. } => {
                wl.destroy();
                _pool.destroy();
            }
        }
    }
}

/// One host output's calloop-side handle: control channel, wake pipe, frames.
struct OutputHandle {
    to_thread: Sender<ToHost>,
    wake: OwnedFd,
    frames: Receiver<HostFrame>,
    /// Frames sent and not yet received, the capture thread adding and the takers below
    /// subtracting; exact, so it may read negative for an instant.
    queued: Arc<AtomicI64>,
    /// Newest frame, kept (slot and all) until replaced so an IDR request on a
    /// static screen can re-encode current content like compositor mode does.
    retained: Mutex<Option<HostFrame>>,
    name: Option<String>,
}

impl OutputHandle {
    fn send(&self, msg: ToHost) {
        let _ = self.to_thread.send(msg);
        wake_write(self.wake.as_raw_fd());
    }
}

#[derive(Clone, Copy)]
struct SerializedMods {
    depressed: u32,
    latched: u32,
    locked: u32,
    group: u32,
}

#[derive(Default)]
struct HostKeyboardState {
    state: Option<xkb::State>,
    pressed: std::collections::HashSet<u32>,
}

impl HostKeyboardState {
    fn set_keymap(&mut self, text: &str) -> Option<SerializedMods> {
        let keymap = crate::wayland::keymap::compile_keymap(text)?;
        // Latched, locked, and the locked layout live in the xkb state rather than the
        // pressed set, so they are read off the outgoing state and seeded back once the
        // held keys are replayed; without this a keymap swap drops CapsLock and a locked
        // group, and the modifiers() that follows clears them on the host.
        let carried = self.state.as_ref().map(|s| {
            (
                s.serialize_mods(xkb::STATE_MODS_LATCHED),
                s.serialize_mods(xkb::STATE_MODS_LOCKED),
                s.serialize_layout(xkb::STATE_LAYOUT_LOCKED),
            )
        });
        let mut state = xkb::State::new(&keymap);
        for &keycode in &self.pressed {
            state.update_key(xkb::Keycode::new(keycode), xkb::KeyDirection::Down);
        }
        if let Some((latched, locked, layout)) = carried {
            let depressed = state.serialize_mods(xkb::STATE_MODS_DEPRESSED);
            state.update_mask(depressed, latched, locked, 0, 0, layout);
        }
        let mods = Self::serialize(&state);
        self.state = Some(state);
        Some(mods)
    }

    fn update_key(&mut self, xkb_keycode: u32, pressed: bool) -> Option<SerializedMods> {
        // A key can only move a state that exists; with no keymap the caller sends no key
        // event, so the pressed set must not record the transition either.
        self.state.as_ref()?;
        let changed = if pressed {
            self.pressed.insert(xkb_keycode)
        } else {
            self.pressed.remove(&xkb_keycode)
        };
        if !changed {
            return None;
        }
        let state = self.state.as_mut()?;
        state.update_key(
            xkb::Keycode::new(xkb_keycode),
            if pressed {
                xkb::KeyDirection::Down
            } else {
                xkb::KeyDirection::Up
            },
        );
        Some(Self::serialize(state))
    }

    /// The keysym `xkb_keycode` carries at the base level of the effective layout, if any.
    fn base_keysym(&self, xkb_keycode: u32) -> Option<u32> {
        let state = self.state.as_ref()?;
        let layout = state.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE);
        let sym = state
            .get_keymap()
            .key_get_syms_by_level(xkb::Keycode::new(xkb_keycode), layout, 0)
            .first()?
            .raw();
        (sym != 0).then_some(sym)
    }

    fn serialize(state: &xkb::State) -> SerializedMods {
        SerializedMods {
            depressed: state.serialize_mods(xkb::STATE_MODS_DEPRESSED),
            latched: state.serialize_mods(xkb::STATE_MODS_LATCHED),
            locked: state.serialize_mods(xkb::STATE_MODS_LOCKED),
            group: state.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE),
        }
    }
}

/// The calloop-side session: input proxies plus one capture handle per output.
pub struct HostSession {
    conn: Connection,
    vk: Option<ZwpVirtualKeyboardV1>,
    keyboard: Mutex<HostKeyboardState>,
    vptr: Option<ZwlrVirtualPointerV1>,
    /// Kernel input devices, taken where `/dev/uinput` can serve them and the
    /// compositor offers neither libei nor a virtual keyboard or pointer, ahead
    /// of the portal's own methods.
    uinput: Option<crate::uinput::Pair>,
    /// The portal session when frames or an input device come through xdg-desktop-portal.
    portal: Option<Arc<PortalCtl>>,
    /// Output geometry by rank, pairing portal streams with outputs.
    geoms: Vec<OutputGeom>,
    ctrl_tx: Sender<CtrlMsg>,
    ctrl_wake: OwnedFd,
    outputs: Vec<OutputHandle>,
    layout: Mutex<std::collections::BTreeMap<u32, LayoutSlot>>,
    /// Rank -> registry index, for looking a display's host output up in `sizes`.
    order: Vec<usize>,
    sizes: Arc<Mutex<Vec<(i32, i32)>>>,
    layouts: Arc<Mutex<LayoutLedger>>,
    alive: Arc<AtomicBool>,
    /// A finger's scroll the uinput wheels have not taken yet, in pixels (`pointer_scroll`).
    scroll_carry: Mutex<(f64, f64)>,
    /// The union-layout point the last absolute motion went to, whose output's scale a
    /// finger's scroll is taken by (`pointer_scroll`).
    pointer_at: Mutex<(f64, f64)>,
}

impl HostSession {
    /// Connect to `display`, bring up input devices, enumerate the host's
    /// outputs, and spawn one capture thread per output (idle until
    /// [`start_capture`]) plus the layout-control thread. `gbm_path` (this
    /// process's render node) enables the zero-copy path; `frame_wake` is told the
    /// output index every time a capture thread queues a frame.
    pub fn connect(
        display: &str,
        gbm_path: Option<std::path::PathBuf>,
        frame_wake: smithay::reexports::calloop::channel::Sender<usize>,
        dma_formats: Vec<(u32, Vec<u64>)>,
        cursor_tx: std::sync::mpsc::Sender<CursorJob>,
    ) -> Result<Self, String> {
        let path = socket_path(display).ok_or("XDG_RUNTIME_DIR is unset")?;
        let stream = UnixStream::connect(&path).map_err(|e| format!("connect {path}: {e}"))?;
        let conn = Connection::from_socket(stream).map_err(|e| format!("wayland setup: {e}"))?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        let _registry = conn.display().get_registry(&qh, ());
        let mut state = CtrlState::default();
        bounded_roundtrip(&conn, &mut queue, &mut state)?;

        let seat = state
            .seat
            .clone()
            .ok_or("host compositor advertises no wl_seat")?;
        if state.outputs.is_empty() {
            return Err("host compositor has no wl_output".into());
        }
        let native_capture =
            state.has_screencopy || (state.has_ext_capture && state.has_ext_source);
        let portal_devices = if state.vk_mgr.is_none() {
            portal::DEVICE_KEYBOARD
        } else {
            0
        } | if state.vptr_mgr.is_none() {
            portal::DEVICE_POINTER
        } else {
            0
        };
        let portal = if !native_capture || portal_devices != 0 {
            match PortalSession::probe() {
                Ok(()) => Some(PortalCtl::spawn(!native_capture, portal_devices)),
                Err(_) if native_capture => None,
                Err(why) => {
                    return Err(format!(
                        "host compositor offers neither ext-image-copy-capture nor zwlr_screencopy_manager_v1 (v3), and no xdg-desktop-portal answers on the session bus ({why})"
                    ));
                }
            }
        } else {
            None
        };
        if !native_capture {
            println!(
                "[HostCapture] host offers no capture protocol; frames come through the xdg-desktop-portal ScreenCast."
            );
        }

        let base_keymap = crate::wayland::vkclient::us_base_text();
        let vk = match &state.vk_mgr {
            Some(mgr) => {
                let vk = mgr.create_virtual_keyboard(&seat, &qh, ());
                // A keymap must precede any key event; selkies replaces this with
                // its managed keymap through the ABI as soon as it starts.
                if let Some(text) = base_keymap {
                    let mut data = text.as_bytes().to_vec();
                    data.push(0);
                    let fd = memfd_with(&data)?;
                    vk.keymap(KEYMAP_FORMAT_XKB_V1, fd.as_fd(), data.len() as u32);
                }
                Some(vk)
            }
            None => {
                eprintln!(
                    "[HostCapture] no zwp_virtual_keyboard_manager_v1: keyboard {}.",
                    if portal.is_some() {
                        "goes through the portal by keysym"
                    } else {
                        "injection disabled"
                    }
                );
                None
            }
        };
        let vptr = match &state.vptr_mgr {
            Some(mgr) => Some(mgr.create_virtual_pointer(Some(&seat), &qh, ())),
            None => {
                eprintln!(
                    "[HostCapture] no zwlr_virtual_pointer_manager_v1: pointer {}.",
                    if portal.is_some() {
                        "goes through the portal"
                    } else {
                        "injection disabled"
                    }
                );
                None
            }
        };
        // Second round-trip: wl_output v4 name events and output-management heads.
        bounded_roundtrip(&conn, &mut queue, &mut state)?;

        let alive = Arc::new(AtomicBool::new(true));
        let mut order: Vec<usize> = (0..state.outputs.len()).collect();
        order.sort_by_key(|&i| output_order_key(state.outputs[i].1.as_ref(), i));
        let names: Vec<Option<String>> =
            order.iter().map(|&i| state.outputs[i].1.clone()).collect();
        let geoms: Vec<OutputGeom> = {
            let sizes = state.sizes.lock().unwrap();
            order
                .iter()
                .map(|&i| {
                    let scale = state.scales.get(i).copied().unwrap_or(1).max(1);
                    let size = sizes.get(i).copied().unwrap_or((0, 0));
                    OutputGeom {
                        pos: state.positions.get(i).copied().unwrap_or((0, 0)),
                        logical: (size.0 / scale, size.1 / scale),
                    }
                })
                .collect()
        };
        state.order = order;
        println!(
            "[HostCapture] host outputs: {}.",
            names
                .iter()
                .enumerate()
                .map(|(i, n)| format!("{i}={}", n.as_deref().unwrap_or("?")))
                .collect::<Vec<_>>()
                .join(", ")
        );

        let mut outputs = Vec::new();
        for (i, name) in names.iter().cloned().enumerate() {
            let (wake_rd, wake_wr) = wake_pipe()?;
            let (to_thread, from_main) = std::sync::mpsc::channel::<ToHost>();
            let (frame_tx, frames) = std::sync::mpsc::channel::<HostFrame>();
            let queued = Arc::new(AtomicI64::new(0));
            let sink = FrameSink {
                tx: frame_tx,
                queued: queued.clone(),
                wake: frame_wake.clone(),
                index: i,
            };
            let display = display.to_string();
            let expect = name.clone();
            let gbm_path = gbm_path.clone();
            let alive = alive.clone();
            let portal_ctl = portal.clone().filter(|_| !native_capture);
            let (dma_formats, cursor_tx, geoms) =
                (dma_formats.clone(), cursor_tx.clone(), geoms.clone());
            std::thread::Builder::new()
                .name(format!("pf-host-cap{i}"))
                .spawn(move || {
                    crate::boost_thread_priority(-10);
                    let outcome = match portal_ctl {
                        Some(ctl) => portal_capture_loop(
                            i,
                            ctl,
                            from_main,
                            sink,
                            cursor_tx,
                            dma_formats,
                            geoms,
                        ),
                        None => {
                            capture_loop(&display, i, expect, gbm_path, from_main, wake_rd, sink)
                        }
                    };
                    if let Err(e) = outcome {
                        eprintln!("[HostCapture] output {i} capture ended: {e}");
                    }
                    if i == 0 {
                        alive.store(false, Ordering::Relaxed);
                    }
                })
                .map_err(|e| format!("spawn: {e}"))?;
            outputs.push(OutputHandle {
                to_thread,
                wake: wake_wr,
                frames,
                queued,
                retained: Mutex::new(None),
                name,
            });
        }

        let (ctrl_tx, ctrl_rx) = std::sync::mpsc::channel::<CtrlMsg>();
        let (ctrl_wake_rd, ctrl_wake) = wake_pipe()?;
        let order_for_session = state.order.clone();
        let sizes = state.sizes.clone();
        let layouts = Arc::new(Mutex::new(LayoutLedger::default()));
        {
            let conn = conn.clone();
            let ledger = layouts.clone();
            let alive = alive.clone();
            std::thread::Builder::new()
                .name("pf-host-ctrl".into())
                .spawn(move || {
                    control_loop(conn, queue, state, ctrl_rx, ctrl_wake_rd, ledger);
                    // The primary connection carries the virtual keyboard and pointer: its
                    // end is the session's end, whatever the capture threads still do.
                    alive.store(false, Ordering::Relaxed);
                })
                .map_err(|e| format!("spawn: {e}"))?;
        }

        let layout = Mutex::new(std::collections::BTreeMap::new());
        let mut keyboard = HostKeyboardState::default();
        let has_keyboard =
            vk.is_some() || portal_devices & portal::DEVICE_KEYBOARD != 0 && portal.is_some();
        if has_keyboard
            && base_keymap
                .and_then(|text| keyboard.set_keymap(text))
                .is_none()
        {
            eprintln!(
                "[HostCapture] base keymap unavailable: keys are dropped until selkies uploads its keymap."
            );
        }
        let uinput = crate::uinput::Pair::open();
        if uinput.is_some() {
            println!("[HostCapture] input goes through kernel uinput devices.");
        }
        Ok(Self {
            conn,
            vk,
            keyboard: Mutex::new(keyboard),
            vptr,
            uinput,
            portal,
            geoms,
            ctrl_tx,
            ctrl_wake,
            outputs,
            layout,
            order: order_for_session,
            sizes,
            layouts,
            alive,
            scroll_carry: Mutex::new((0.0, 0.0)),
            pointer_at: Mutex::new((0.0, 0.0)),
        })
    }

    pub fn output_count(&self) -> usize {
        self.outputs.len()
    }

    /// The host output backing `display_id`: active display ids map onto host
    /// outputs by ascending rank (selkies ids are sparse — 'display2' is id 2).
    fn output_index_for(&self, display_id: u32) -> Option<usize> {
        let layout = self.layout.lock().unwrap();
        let rank = layout
            .iter()
            .filter(|(_, s)| s.active)
            .position(|(id, _)| *id == display_id)?;
        (rank < self.outputs.len()).then_some(rank)
    }

    /// True when an active capture on `display_id` has a host output behind it.
    pub fn has_output_for(&self, display_id: u32) -> bool {
        self.output_index_for(display_id).is_some()
    }

    /// The display captured from host output `index`, if an active capture sits on it.
    pub fn display_for_output(&self, index: usize) -> Option<u32> {
        let layout = self.layout.lock().unwrap();
        layout
            .iter()
            .filter(|(_, s)| s.active)
            .nth(index)
            .map(|(id, _)| *id)
    }

    /// Whether host output `index` has queued a frame no render has taken yet.
    pub fn has_queued_frame(&self, index: usize) -> bool {
        self.outputs
            .get(index)
            .is_some_and(|h| h.queued.load(Ordering::Acquire) > 0)
    }

    /// Record where selkies laid out `display_id` (union coordinates); pushed
    /// to the host with the next capture (re)start.
    pub fn set_layout(&self, display_id: u32, x: i32, y: i32) {
        let mut layout = self.layout.lock().unwrap();
        layout.entry(display_id).or_default().pos = (x, y);
    }

    /// The current mode of the host output backing `display_id` (physical
    /// pixels), as the host last announced it — what a capture follows when the
    /// host keeps its own mode instead of taking the requested one.
    pub fn current_output_size(&self, display_id: u32) -> Option<(i32, i32)> {
        let rank = self.output_index_for(display_id)?;
        let registry_idx = *self.order.get(rank)?;
        let size = *self.sizes.lock().unwrap().get(registry_idx)?;
        (size.0 > 0 && size.1 > 0).then_some(size)
    }

    /// Number a layout request and hand it to the control thread.
    fn send_apply(&self, slots: Vec<LayoutSlot>) -> u64 {
        let epoch = self.layouts.lock().unwrap().issue();
        let _ = self.ctrl_tx.send(CtrlMsg::Apply { epoch, slots });
        wake_write(self.ctrl_wake.as_raw_fd());
        epoch
    }

    /// The host's answer to the layout request `epoch` (as returned by [`start_capture`]):
    /// `Some(true)` once the host said it applied the request, `Some(false)` once it said it
    /// did not (refusal, no layout management at all — KWin —, or no answer within
    /// `LAYOUT_DEADLINE`), `None` while still unanswered. Never blocks. Which of the two it
    /// is settles nothing on its own, since a host may acknowledge a mode it did not take:
    /// the caller polls for either answer, then compares [`current_output_size`] with what
    /// it asked for.
    pub fn layout_outcome(&self, epoch: u64) -> Option<bool> {
        self.layouts.lock().unwrap().outcome(epoch)
    }

    /// Aim `display_id` at `width`x`height`: assigns every active display its
    /// host output (by rank), asks the host for all modes and positions in one
    /// atomic configuration, and points the capture threads at their sizes and
    /// buffer types; frames gate until the host applies them. `zero_copy` states
    /// whether this display's encoder imports dmabufs — a CPU consumer needs shm
    /// frames it can read, and gets them even on a GPU-capable host. Returns the
    /// layout request's epoch for [`layout_outcome`]; nothing here waits on the host.
    pub fn start_capture(
        &self,
        display_id: u32,
        width: i32,
        height: i32,
        zero_copy: bool,
        paint_cursor: bool,
        fps: f64,
    ) -> u64 {
        if let Some(ctl) = &self.portal {
            ctl.set_paint(paint_cursor);
        }
        let assignments = {
            let mut layout = self.layout.lock().unwrap();
            let slot = layout.entry(display_id).or_default();
            slot.want = (width, height);
            slot.zero_copy = zero_copy;
            slot.paint_cursor = paint_cursor;
            slot.fps_milli = fps_milli(fps);
            slot.active = true;
            let active: Vec<(u32, LayoutSlot)> = layout
                .iter()
                .filter(|(_, s)| s.active)
                .map(|(id, s)| (*id, *s))
                .collect();
            if active
                .iter()
                .position(|(id, _)| *id == display_id)
                .unwrap_or(usize::MAX)
                >= self.outputs.len()
            {
                eprintln!(
                    "[HostCapture] display {display_id} has no host output (host has {}); not captured.",
                    self.outputs.len()
                );
            }
            active
        };
        // Ranks may have shifted (a lower id joined): repoint every assigned
        // capture thread; a same-size Start is a no-op for an unaffected one.
        let mut by_output: Vec<LayoutSlot> = Vec::new();
        for (rank, (_, slot)) in assignments.iter().enumerate() {
            if rank >= self.outputs.len() {
                break;
            }
            // Frames buffered at another size stay: the capture consumes only frames of its own
            // size, and a host that keeps its own mode brings the capture back to theirs.
            self.outputs[rank].send(slot.start());
            by_output.push(*slot);
        }
        self.send_apply(by_output)
    }

    /// Point `display_id`'s capture at dmabufs (`zero_copy`) or shm frames without touching its
    /// mode or position, which is what an encoder demoted or rebuilt mid-capture needs: the
    /// layout is already what the host applied, and re-running it would mode-set the host
    /// display again. Idempotent, so a consumer that keeps seeing the wrong buffer type until
    /// the capture thread reallocates can call this on every frame.
    pub fn set_buffer_type(&self, display_id: u32, zero_copy: bool) {
        let target = {
            let mut layout = self.layout.lock().unwrap();
            let slot = layout.entry(display_id).or_default();
            if slot.zero_copy == zero_copy {
                return;
            }
            slot.zero_copy = zero_copy;
            let (start, active) = (slot.start(), slot.active);
            let rank = layout
                .iter()
                .filter(|(_, s)| s.active)
                .position(|(id, _)| *id == display_id);
            match rank {
                Some(r) if active && r < self.outputs.len() => (r, start),
                _ => return,
            }
        };
        let (idx, start) = target;
        // Same geometry, new buffer type: a retained dmabuf cannot be consumed
        // by the shm path (nor vice versa), so everything buffered goes back.
        self.drop_buffered_frames(idx);
        self.outputs[idx].send(start);
    }

    /// Point `display_id`'s capture at a new rate: what a portal stream asks the compositor
    /// to hold; the Wayland capture protocols are demand-driven and ignore it.
    pub fn set_fps(&self, display_id: u32, fps: f64) {
        let target = {
            let mut layout = self.layout.lock().unwrap();
            let slot = layout.entry(display_id).or_default();
            let milli = fps_milli(fps);
            if slot.fps_milli == milli {
                return;
            }
            slot.fps_milli = milli;
            let (start, active) = (slot.start(), slot.active);
            let rank = layout
                .iter()
                .filter(|(_, s)| s.active)
                .position(|(id, _)| *id == display_id);
            match rank {
                Some(r) if active && r < self.outputs.len() => (r, start),
                _ => return,
            }
        };
        self.outputs[target.0].send(target.1);
    }

    /// Switch every active capture between host-painted and consumer-drawn cursors
    /// (the consumer's native cursor rendering toggle), keeping geometry and buffer type.
    /// Frames already buffered stay valid either way. Idempotent.
    pub fn set_cursor_painting(&self, paint_cursor: bool) {
        if let Some(ctl) = &self.portal {
            ctl.set_paint(paint_cursor);
        }
        let targets: Vec<(usize, LayoutSlot)> = {
            let mut layout = self.layout.lock().unwrap();
            let mut out = Vec::new();
            for (rank, (_, slot)) in layout.iter_mut().filter(|(_, s)| s.active).enumerate() {
                if rank >= self.outputs.len() {
                    break;
                }
                if slot.paint_cursor != paint_cursor {
                    slot.paint_cursor = paint_cursor;
                    out.push((rank, *slot));
                }
            }
            out
        };
        for (idx, slot) in targets {
            self.outputs[idx].send(slot.start());
        }
    }

    /// Drop every buffered frame for an output: a buffer-type change or a parked
    /// output leaves them without a consumer. The slots go straight back to the
    /// pool; dropping them silently would wedge a slot until the next
    /// renegotiation.
    fn drop_buffered_frames(&self, idx: usize) {
        let handle = &self.outputs[idx];
        if let Some(old) = handle.retained.lock().unwrap().take() {
            handle.send(ToHost::Release {
                generation: old.generation,
                slot: old.slot,
            });
        }
        while let Ok(frame) = handle.frames.try_recv() {
            handle.queued.fetch_sub(1, Ordering::AcqRel);
            handle.send(ToHost::Release {
                generation: frame.generation,
                slot: frame.slot,
            });
        }
    }

    /// Park `display_id`'s capture (its slot leaves the pointer extent). The
    /// retained frame and anything still queued go back to the capture pool —
    /// an idle output has no consumer for them, and dropping them silently
    /// would wedge slots until the next renegotiation.
    pub fn idle_output(&self, display_id: u32) {
        let Some(idx) = self.output_index_for(display_id) else {
            self.layout
                .lock()
                .unwrap()
                .entry(display_id)
                .or_default()
                .active = false;
            return;
        };
        self.layout
            .lock()
            .unwrap()
            .entry(display_id)
            .or_default()
            .active = false;
        self.outputs[idx].send(ToHost::Idle);
        self.drop_buffered_frames(idx);
    }

    /// False once the host connection is gone (the primary connection or the first
    /// output's capture ended): the owner drops the session and reports its captures as
    /// stopped, so they are rebuilt against a new connection instead of feeding a dead one.
    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed) && !self.portal.as_ref().is_some_and(|c| c.dead())
    }

    /// Hand one frame back to the pool (same path the drain/idle releases take).
    pub fn release_frame(&self, display_id: u32, frame: HostFrame) {
        if let Some(idx) = self.output_index_for(display_id) {
            self.outputs[idx].send(ToHost::Release {
                generation: frame.generation,
                slot: frame.slot,
            });
        }
    }

    /// Newest ready frame for `display_id`, releasing any staler ones straight
    /// back to the pool.
    pub fn try_take_frame(&self, display_id: u32) -> Option<HostFrame> {
        let handle = self.outputs.get(self.output_index_for(display_id)?)?;
        let mut newest: Option<HostFrame> = None;
        while let Ok(frame) = handle.frames.try_recv() {
            handle.queued.fetch_sub(1, Ordering::AcqRel);
            if let Some(stale) = newest.replace(frame) {
                handle.send(ToHost::Release {
                    generation: stale.generation,
                    slot: stale.slot,
                });
            }
        }
        newest
    }

    /// Keep `frame` as `display_id`'s current content (releasing the one it
    /// replaces).
    pub fn retain_frame(&self, display_id: u32, frame: HostFrame) {
        let Some(handle) = self
            .output_index_for(display_id)
            .and_then(|i| self.outputs.get(i))
        else {
            return;
        };
        let old = handle.retained.lock().unwrap().replace(frame);
        if let Some(old) = old {
            handle.send(ToHost::Release {
                generation: old.generation,
                slot: old.slot,
            });
        }
    }

    /// Run `f` with `display_id`'s retained (current-content) frame, if any.
    pub fn with_retained<R>(&self, display_id: u32, f: impl FnOnce(Option<&HostFrame>) -> R) -> R {
        match self
            .output_index_for(display_id)
            .and_then(|i| self.outputs.get(i))
        {
            Some(handle) => f(handle.retained.lock().unwrap().as_ref()),
            None => f(None),
        }
    }

    /// Take selkies' managed keymap: uploaded verbatim to the virtual keyboard, and the
    /// table portal keys resolve their keysyms from.
    pub fn set_keymap(&self, text: &str) {
        let mut keyboard = self.keyboard.lock().unwrap();
        let Some(mods) = keyboard.set_keymap(text) else {
            eprintln!("[HostCapture] keymap state update failed");
            return;
        };
        let Some(vk) = &self.vk else { return };
        let mut data = text.as_bytes().to_vec();
        data.push(0);
        let fd = match memfd_with(&data) {
            Ok(fd) => fd,
            Err(e) => {
                eprintln!("[HostCapture] keymap upload failed: {e}");
                return;
            }
        };
        vk.keymap(KEYMAP_FORMAT_XKB_V1, fd.as_fd(), data.len() as u32);
        vk.modifiers(mods.depressed, mods.latched, mods.locked, mods.group);
        let _ = self.conn.flush();
    }

    /// Key event in xkb numbering (evdev + 8), matching the seat injectors. The virtual
    /// keyboard takes the keycode under selkies' keymap; the portal takes the keysym that
    /// keycode carries at its base level, which the host resolves in its own layout, and the
    /// evdev code only for a key without one.
    pub fn key(&self, xkb_keycode: u32, pressed: bool) {
        if xkb_keycode < 8 {
            return;
        }
        let mut keyboard = self.keyboard.lock().unwrap();
        let Some(mods) = keyboard.update_key(xkb_keycode, pressed) else {
            return;
        };
        let sym = keyboard.base_keysym(xkb_keycode);
        drop(keyboard);
        if let Some(live) = self.portal_ei() {
            live.ei.as_ref().unwrap().key(xkb_keycode, sym, pressed);
            return;
        }
        if let Some(vk) = &self.vk {
            vk.key(0, xkb_keycode - 8, if pressed { 1 } else { 0 });
            vk.modifiers(mods.depressed, mods.latched, mods.locked, mods.group);
            let _ = self.conn.flush();
            return;
        }
        if let Some(u) = &self.uinput {
            let _ = u.keyboard.emit(
                crate::uinput::EV_KEY,
                (xkb_keycode - 8) as u16,
                if pressed { 1 } else { 0 },
            );
            return;
        }
        self.with_portal(|p| match sym {
            Some(sym) => p.keysym(sym as i32, pressed),
            None => p.keycode(xkb_keycode as i32 - 8, pressed),
        });
    }

    /// Run `f` with the portal session while one is live.
    fn with_portal(&self, f: impl FnOnce(&PortalSession)) {
        if let Some(live) = self.portal.as_ref().and_then(|c| c.live()) {
            f(&live.session);
        }
    }

    /// The live portal session's libei injector, while one is up: input takes it over the
    /// portal's `Notify*` methods where the backend granted an EIS socket.
    fn portal_ei(&self) -> Option<Arc<PortalLive>> {
        self.portal
            .as_ref()
            .and_then(|c| c.live())
            .filter(|l| l.ei.as_ref().is_some_and(|e| e.alive()))
    }

    /// Union-layout bounding box of every active, output-backed display (the
    /// virtual-pointer extent, matching the space selkies injects in).
    fn extent(&self) -> (i32, i32) {
        let layout = self.layout.lock().unwrap();
        let mut w = 0;
        let mut h = 0;
        for (rank, (_, slot)) in layout.iter().filter(|(_, s)| s.active).enumerate() {
            if rank >= self.outputs.len() {
                break;
            }
            w = w.max(slot.pos.0 + slot.want.0);
            h = h.max(slot.pos.1 + slot.want.1);
        }
        (w, h)
    }

    /// The portal stream under a union-layout point and where the point lands on it: the active
    /// display whose slot holds the point (else the nearest slot), paired with its stream by
    /// rank.
    fn portal_target(&self, x: f64, y: f64) -> Option<PortalPoint> {
        let live = self.portal.as_ref()?.live()?;
        let slots = self.active_slots();
        let rank = nearest_slot(&slots, x, y)?;
        let assignment = assign_streams(&live.session.streams, &self.geoms);
        let stream = &live.session.streams[(*assignment.get(rank)?)?];
        let slot = &slots[rank];
        Some(PortalPoint {
            node: stream.node_id,
            logical: stream_point(slot, stream.size.unwrap_or(slot.want), x, y),
            position: stream.position,
        })
    }

    /// The active, output-backed displays' slots, by rank.
    fn active_slots(&self) -> Vec<LayoutSlot> {
        let layout = self.layout.lock().unwrap();
        layout
            .values()
            .filter(|s| s.active)
            .copied()
            .take(self.outputs.len())
            .collect()
    }

    /// The scale of the host output under a union-layout point: its display's capture size
    /// over the logical size the portal's stream announces for it, else the output's mode
    /// over its integer scale.
    fn scale_at(&self, x: f64, y: f64) -> f64 {
        let slots = self.active_slots();
        let Some(rank) = nearest_slot(&slots, x, y) else {
            return 1.0;
        };
        let logical = self
            .portal
            .as_ref()
            .and_then(|c| c.live())
            .and_then(|live| {
                let assignment = assign_streams(&live.session.streams, &self.geoms);
                live.session.streams[(*assignment.get(rank)?)?].size
            })
            .or_else(|| self.geoms.get(rank).map(|g| g.logical));
        match logical {
            Some((w, _)) if w > 0 && slots[rank].want.0 > 0 => slots[rank].want.0 as f64 / w as f64,
            _ => 1.0,
        }
    }

    /// Absolute motion to a union-layout point, the physical space selkies injects in. libei
    /// takes it in the compositor's logical space, where EIS regions lie: the stream's
    /// position plus the point in its logical size. A union layout with no stream position
    /// to place it by goes as it is, which is that space on an unscaled host.
    pub fn pointer_motion_abs(&self, x: f64, y: f64) {
        *self.pointer_at.lock().unwrap() = (x, y);
        if let Some(live) = self.portal_ei() {
            let ei = live.ei.as_ref().unwrap();
            if let Some(PortalPoint {
                logical,
                position: Some(pos),
                ..
            }) = self.portal_target(x, y)
            {
                ei.pointer_motion_abs(pos.0 as f64 + logical.0, pos.1 as f64 + logical.1);
                return;
            }
            let (w, h) = self.extent();
            if w > 0 && h > 0 {
                ei.pointer_motion_abs(x.clamp(0.0, (w - 1) as f64), y.clamp(0.0, (h - 1) as f64));
            }
            return;
        }
        let Some(vp) = &self.vptr else {
            if let Some(u) = &self.uinput {
                let (w, h) = self.extent();
                if w > 1 && h > 1 {
                    let scale = |v: f64, span: i32| {
                        (v.clamp(0.0, (span - 1) as f64) / (span - 1) as f64
                            * crate::uinput::ABS_RANGE as f64)
                            .round() as i32
                    };
                    let _ =
                        u.pointer
                            .emit(crate::uinput::EV_ABS, crate::uinput::ABS_X, scale(x, w));
                    let _ =
                        u.pointer
                            .emit(crate::uinput::EV_ABS, crate::uinput::ABS_Y, scale(y, h));
                }
            } else if let Some(t) = self.portal_target(x, y) {
                self.with_portal(|p| p.pointer_motion_abs(t.node, t.logical.0, t.logical.1));
            }
            return;
        };
        let (w, h) = self.extent();
        if w <= 0 || h <= 0 {
            return;
        }
        let cx = x.clamp(0.0, (w - 1) as f64) as u32;
        let cy = y.clamp(0.0, (h - 1) as f64) as u32;
        vp.motion_absolute(0, cx, cy, w as u32, h as u32);
        vp.frame();
        let _ = self.conn.flush();
    }

    /// Relative motion in union-layout units, sent as the host's own relative motion: the
    /// host moves its pointer by the delta and clamps it to its outputs, or, for a
    /// pointer-locked client, passes the delta on without moving it. An absolute warp
    /// against a position tracked here could not do the second: the locked host pointer
    /// stays put, so every warp would read as the whole distance from the lock point.
    pub fn pointer_motion_rel(&self, dx: f64, dy: f64) {
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        if let Some(live) = self.portal_ei() {
            live.ei.as_ref().unwrap().pointer_motion(dx, dy);
            return;
        }
        let Some(vp) = &self.vptr else {
            if let Some(u) = &self.uinput {
                let _ = u.pointer.emit(
                    crate::uinput::EV_REL,
                    crate::uinput::REL_X,
                    dx.round() as i32,
                );
                let _ = u.pointer.emit(
                    crate::uinput::EV_REL,
                    crate::uinput::REL_Y,
                    dy.round() as i32,
                );
            } else {
                self.with_portal(|p| p.pointer_motion(dx, dy));
            }
            return;
        };
        vp.motion(0, dx, dy);
        vp.frame();
        let _ = self.conn.flush();
    }

    pub fn pointer_button(&self, btn: u32, pressed: bool) {
        if let Some(live) = self.portal_ei() {
            live.ei
                .as_ref()
                .unwrap()
                .pointer_button(btn as i32, pressed);
            return;
        }
        let Some(vp) = &self.vptr else {
            if let Some(u) = &self.uinput {
                let _ = u.pointer.emit(
                    crate::uinput::EV_KEY,
                    btn as u16,
                    if pressed { 1 } else { 0 },
                );
            } else {
                self.with_portal(|p| p.pointer_button(btn as i32, pressed));
            }
            return;
        };
        vp.button(
            0,
            btn,
            if pressed {
                wl_pointer::ButtonState::Pressed
            } else {
                wl_pointer::ButtonState::Released
            },
        );
        vp.frame();
        let _ = self.conn.flush();
    }

    /// Wheel scroll: the continuous value plus the discrete step count the seat path reports
    /// as v120, so host clients that scroll by notches (terminals, games, GTK list views)
    /// see the same steps as in nested mode; a value below one notch stays continuous. The
    /// source follows each axis request, which is the axis it applies to on wlroots.
    pub fn pointer_axis(&self, dx: f64, dy: f64) {
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        let steps = |value: f64| (value * crate::SCROLL_V120_PER_UNIT / 120.0).round() as i32;
        let scroll =
            |axis: u32, value: f64, cont: &dyn Fn(f64, f64, bool), disc: &dyn Fn(u32, i32)| {
                match steps(value) {
                    0 => cont(
                        if axis == 1 { value } else { 0.0 },
                        if axis == 0 { value } else { 0.0 },
                        true,
                    ),
                    n => disc(axis, n),
                }
            };
        if let Some(live) = self.portal_ei() {
            let ei = live.ei.as_ref().unwrap();
            for (axis, value) in [(0u32, dy), (1u32, dx)] {
                if value != 0.0 {
                    scroll(axis, value, &|x, y, f| ei.pointer_axis(x, y, f), &|a, n| {
                        ei.pointer_axis_discrete(a, n)
                    });
                }
            }
            return;
        }
        let Some(vp) = &self.vptr else {
            if let Some(u) = &self.uinput {
                // A wheel counts up where the axis counts down, so the vertical step is negated.
                for (code, value, sign) in [
                    (crate::uinput::REL_WHEEL, dy, -1),
                    (crate::uinput::REL_HWHEEL, dx, 1),
                ] {
                    match steps(value) {
                        0 => {}
                        n => {
                            let _ = u.pointer.emit(crate::uinput::EV_REL, code, n * sign);
                        }
                    }
                }
            } else {
                self.with_portal(|p| {
                    for (axis, value) in [(0u32, dy), (1u32, dx)] {
                        if value != 0.0 {
                            scroll(axis, value, &|x, y, f| p.pointer_axis(x, y, f), &|a, n| {
                                p.pointer_axis_discrete(a, n)
                            });
                        }
                    }
                });
            }
            return;
        };
        for (axis, value) in [
            (wl_pointer::Axis::VerticalScroll, dy),
            (wl_pointer::Axis::HorizontalScroll, dx),
        ] {
            if value == 0.0 {
                continue;
            }
            let steps = steps(value);
            if steps != 0 {
                vp.axis_discrete(0, axis, value, steps);
            } else {
                vp.axis(0, axis, value);
            }
            vp.axis_source(wl_pointer::AxisSource::Wheel);
        }
        vp.frame();
        let _ = self.conn.flush();
    }
}

/// Pixels of a finger's scroll per wheel notch where only notches exist (the uinput wheels):
/// the size Selkies' page takes for a touchpad's notch.
const SCROLL_PX_PER_NOTCH: f64 = 100.0;

/// The whole notches in `carry + delta` pixels, the fraction left in `carry`.
fn scroll_notches(carry: &mut f64, delta: f64) -> i32 {
    *carry += delta;
    let notches = (*carry / SCROLL_PX_PER_NOTCH).trunc();
    *carry -= notches * SCROLL_PX_PER_NOTCH;
    notches as i32
}

impl HostSession {
    /// A finger's scroll by (dx, dy) pixels of the stream on the host, in the logical pixels of
    /// the output under the pointer as the nested compositor takes it: smooth, as a finger
    /// source ended by `pointer_scroll_end`, through libei, the portal or the virtual pointer,
    /// and in whole wheel notches with the remainder carried on uinput, whose wheels have no
    /// other.
    pub fn pointer_scroll(&self, dx: f64, dy: f64) {
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        let (px, py) = *self.pointer_at.lock().unwrap();
        let scale = self.scale_at(px, py);
        let (dx, dy) = (dx / scale, dy / scale);
        if let Some(live) = self.portal_ei() {
            live.ei.as_ref().unwrap().pointer_axis(dx, dy, false);
            return;
        }
        let Some(vp) = &self.vptr else {
            if let Some(u) = &self.uinput {
                let mut carry = self.scroll_carry.lock().unwrap();
                // A wheel counts up where the axis counts down, so the vertical step is negated.
                let (wheel, hwheel) = (
                    scroll_notches(&mut carry.1, dy),
                    scroll_notches(&mut carry.0, dx),
                );
                if wheel != 0 {
                    let _ = u
                        .pointer
                        .emit(crate::uinput::EV_REL, crate::uinput::REL_WHEEL, -wheel);
                }
                if hwheel != 0 {
                    let _ =
                        u.pointer
                            .emit(crate::uinput::EV_REL, crate::uinput::REL_HWHEEL, hwheel);
                }
            } else {
                self.with_portal(|p| p.pointer_axis(dx, dy, false));
            }
            return;
        };
        vp.axis_source(wl_pointer::AxisSource::Finger);
        for (axis, value) in [
            (wl_pointer::Axis::VerticalScroll, dy),
            (wl_pointer::Axis::HorizontalScroll, dx),
        ] {
            if value != 0.0 {
                vp.axis(0, axis, value);
            }
        }
        vp.frame();
        let _ = self.conn.flush();
    }

    /// The finger lifted: the stop each smooth rung takes; uinput drops what it carried.
    pub fn pointer_scroll_end(&self) {
        if let Some(live) = self.portal_ei() {
            live.ei.as_ref().unwrap().pointer_axis(0.0, 0.0, true);
            return;
        }
        let Some(vp) = &self.vptr else {
            if self.uinput.is_some() {
                *self.scroll_carry.lock().unwrap() = (0.0, 0.0);
            } else {
                self.with_portal(|p| p.pointer_axis(0.0, 0.0, true));
            }
            return;
        };
        vp.axis_source(wl_pointer::AxisSource::Finger);
        vp.axis_stop(0, wl_pointer::Axis::VerticalScroll);
        vp.axis_stop(0, wl_pointer::Axis::HorizontalScroll);
        vp.frame();
        let _ = self.conn.flush();
    }
}

impl Drop for HostSession {
    fn drop(&mut self) {
        if let Some(ctl) = &self.portal {
            ctl.close();
        }
    }
}

/// A capture rate as the millihertz a portal stream is asked to hold.
fn fps_milli(fps: f64) -> u32 {
    (fps.max(1.0) * 1000.0).round() as u32
}

impl LayoutSlot {
    /// The Start message aiming a capture thread at this slot.
    fn start(&self) -> ToHost {
        ToHost::Start {
            width: self.want.0,
            height: self.want.1,
            zero_copy: self.zero_copy,
            paint_cursor: self.paint_cursor,
            fps_milli: self.fps_milli,
        }
    }
}

/// The active slot holding `(x, y)`, else the one nearest to it, by rank.
fn nearest_slot(slots: &[LayoutSlot], x: f64, y: f64) -> Option<usize> {
    slots
        .iter()
        .enumerate()
        .map(|(rank, s)| {
            let cx = x.clamp(s.pos.0 as f64, (s.pos.0 + s.want.0).max(s.pos.0) as f64);
            let cy = y.clamp(s.pos.1 as f64, (s.pos.1 + s.want.1).max(s.pos.1) as f64);
            (rank, (cx - x).powi(2) + (cy - y).powi(2))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(rank, _)| rank)
}

/// Where a union-layout point lands on the portal stream under it (`HostSession::portal_target`).
struct PortalPoint {
    node: u32,
    /// In the stream's logical space, the size the portal announced for it.
    logical: (f64, f64),
    /// The stream's position in the compositor's logical space, where the portal gave one.
    position: Option<(i32, i32)>,
}

/// A union-layout point in the logical space of the stream behind `slot`: the slot captures
/// its output at physical size `want`, the portal addresses it at logical size `logical`.
fn stream_point(slot: &LayoutSlot, logical: (i32, i32), x: f64, y: f64) -> (f64, f64) {
    let scale = |v: f64, from: i32, to: i32| {
        let mapped = if from > 0 {
            v * to as f64 / from as f64
        } else {
            v
        };
        mapped.clamp(0.0, (to.max(1) - 1) as f64)
    };
    (
        scale(x - slot.pos.0 as f64, slot.want.0, logical.0),
        scale(y - slot.pos.1 as f64, slot.want.1, logical.1),
    )
}

/// Wait, bounded, until the GPU work the compositor queued on a host dmabuf has finished:
/// the CUDA import reads the buffer without honoring its implicit fences, so a frame
/// encoded the instant it was announced could otherwise carry a half-blitted image. Drivers
/// that attach no fence return at once.
pub fn wait_gpu_done(dmabuf: &Dmabuf) {
    for fd in dmabuf.handles() {
        let mut pfd = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        unsafe { libc::poll(&mut pfd, 1, 50) };
    }
}

// ---------------------------------------------------------------------------
// Portal rung: the session-wide portal, and the stream consumer per output.
// ---------------------------------------------------------------------------

/// One host output's placement, for pairing portal streams with outputs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct OutputGeom {
    /// Layout position in the compositor's logical space.
    pos: (i32, i32),
    /// Mode divided by the integer scale.
    logical: (i32, i32),
}

/// Which portal stream backs each output rank: the stream announcing the output's position,
/// else the one announcing its logical size when that is unambiguous, else the stream at the
/// same index (a backend that sends no geometry lists monitors in its output order).
fn assign_streams(streams: &[PortalStream], geoms: &[OutputGeom]) -> Vec<Option<usize>> {
    let mut taken = vec![false; streams.len()];
    let mut out = vec![None; geoms.len()];
    for (rank, g) in geoms.iter().enumerate() {
        if let Some(i) = streams.iter().position(|s| s.position == Some(g.pos))
            && !taken[i]
        {
            out[rank] = Some(i);
            taken[i] = true;
        }
    }
    for (rank, g) in geoms.iter().enumerate() {
        if out[rank].is_some() {
            continue;
        }
        let mut same_size = streams
            .iter()
            .enumerate()
            .filter(|(i, s)| !taken[*i] && s.position.is_none() && s.size == Some(g.logical));
        if let (Some((i, _)), None) = (same_size.next(), same_size.next()) {
            out[rank] = Some(i);
            taken[i] = true;
        }
    }
    for (rank, slot) in out.iter_mut().enumerate() {
        if slot.is_none() && rank < streams.len() && !taken[rank] {
            *slot = Some(rank);
            taken[rank] = true;
        }
    }
    out
}

/// A live portal session: the D-Bus session, and the PipeWire connection its streams live on
/// when frames come through it.
pub(crate) struct PortalLive {
    pub(crate) session: PortalSession,
    pub(crate) core: Option<Arc<PwCore>>,
    /// Numbers the session so an output thread notices a reopened one.
    pub(crate) epoch: u64,
    /// The cursor painting this session was opened for.
    paint: bool,
    /// The libei injector when the portal handed out an EIS socket; input then rides it
    /// instead of the portal's `Notify*` methods, which the session refuses once EIS is on.
    ei: Option<EiInjector>,
}

#[derive(Default)]
struct PortalInner {
    /// The cursor painting the next session opens with; none until the first Start decides.
    paint: Option<bool>,
    live: Option<Arc<PortalLive>>,
    epoch: u64,
    restore_token: Option<String>,
    dead: Option<String>,
    closed: bool,
}

/// Session-wide portal state: the session thread opens (and, when the cursor mode changes,
/// reopens) the portal session; the outputs and the input methods read the live one.
pub(crate) struct PortalCtl {
    inner: Mutex<PortalInner>,
    changed: Condvar,
    capture: bool,
    devices: u32,
}

impl PortalCtl {
    fn spawn(capture: bool, devices: u32) -> Arc<Self> {
        let ctl = Arc::new(PortalCtl {
            inner: Mutex::new(PortalInner::default()),
            changed: Condvar::new(),
            capture,
            devices,
        });
        let worker = ctl.clone();
        let _ = std::thread::Builder::new()
            .name("pf-host-portal".into())
            .spawn(move || portal_thread(worker));
        ctl
    }

    /// The cursor painting the session should carry (embedded cursor, or cursor metadata
    /// for the consumer to draw); a change reopens the session.
    fn set_paint(&self, paint: bool) {
        let mut inner = self.inner.lock().unwrap();
        if inner.paint != Some(paint) {
            inner.paint = Some(paint);
            self.changed.notify_all();
        }
    }

    pub(crate) fn live(&self) -> Option<Arc<PortalLive>> {
        self.inner.lock().unwrap().live.clone()
    }

    fn dead(&self) -> bool {
        self.inner.lock().unwrap().dead.is_some()
    }

    fn mark_dead(&self, why: String) {
        let mut inner = self.inner.lock().unwrap();
        if inner.dead.is_none() {
            eprintln!("[HostCapture] portal session lost: {why}");
            inner.dead = Some(why);
        }
        inner.live = None;
        self.changed.notify_all();
    }

    fn close(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.closed = true;
        inner.live = None;
        self.changed.notify_all();
    }
}

/// The portal thread: opens the session once the first Start has decided the cursor mode,
/// reopens it (restoring with the token the backend gave) whenever that mode changes, and
/// ends with the host session.
fn portal_thread(ctl: Arc<PortalCtl>) {
    loop {
        let (paint, token) = {
            let mut inner = ctl.inner.lock().unwrap();
            loop {
                if inner.closed || inner.dead.is_some() {
                    return;
                }
                if let Some(p) = inner.paint
                    && inner.live.as_ref().is_none_or(|l| l.paint != p)
                {
                    break (
                        p,
                        inner
                            .restore_token
                            .clone()
                            .or_else(portal::saved_restore_token),
                    );
                }
                inner = ctl.changed.wait(inner).unwrap();
            }
        };
        // The outgoing session closes outside the lock; its Close is what stops the backend's
        // streams before the new session asks for them again.
        let old = ctl.inner.lock().unwrap().live.take();
        drop(old);
        let cursor_mode = if ctl.capture && !paint {
            portal::CURSOR_METADATA
        } else {
            portal::CURSOR_EMBEDDED
        };
        let open = |devices: u32| {
            PortalSession::open(ctl.capture, devices, cursor_mode, token.as_deref()).and_then(
                |session| {
                    let core = if ctl.capture {
                        Some(PwCore::connect(session.open_pipewire_remote()?)?)
                    } else {
                        None
                    };
                    Ok((session, core))
                },
            )
        };
        // A host that grants the screen but refuses the input devices answers the whole request as
        // a refusal, so asking again for capture alone is what turns that into a session with
        // video and no injection, rather than no session at all.
        let opened = match open(ctl.devices) {
            Err(e) if ctl.capture && ctl.devices != 0 => {
                eprintln!(
                    "[HostCapture] portal refused keyboard and pointer ({e}); capturing without them."
                );
                open(0)
            }
            other => other,
        };
        let mut inner = ctl.inner.lock().unwrap();
        match opened {
            Ok((session, core)) => {
                inner.epoch += 1;
                if session.restore_token.is_some() {
                    inner.restore_token = session.restore_token.clone();
                }
                // Prefer libei where the backend offers it: the injector is kept only once its
                // handshake binds a device, and a successful ConnectToEIS makes the session
                // refuse the Notify* methods, so the choice stands for the session's life.
                let ei = (session.devices != 0 && session.eis_capable())
                    .then(|| open_eis(&session))
                    .flatten();
                println!(
                    "[HostCapture] portal session open: {} stream(s), devices {}{}, cursor {}, input via {}.",
                    session.streams.len(),
                    if session.devices & portal::DEVICE_KEYBOARD != 0 {
                        "keyboard "
                    } else {
                        ""
                    },
                    if session.devices & portal::DEVICE_POINTER != 0 {
                        "pointer"
                    } else {
                        ""
                    },
                    match session.cursor_mode {
                        portal::CURSOR_METADATA => "metadata",
                        portal::CURSOR_EMBEDDED => "embedded",
                        _ => "hidden",
                    },
                    if ei.is_some() {
                        "libei"
                    } else {
                        "portal notify"
                    }
                );
                // Mutter divides the spec's logical point by the monitor's scale, and the frontend
                // refuses one past the logical size, so only the backend can convert it.
                if ei.is_none() && session.mutter && session.devices & portal::DEVICE_POINTER != 0 {
                    eprintln!(
                        "[HostCapture] Mutter takes the portal's absolute pointer motion in the stream's pixels: on a scaled monitor the pointer lands at 1/scale of where it points unless the portal backend converts it, which xdg-desktop-portal-gnome does not."
                    );
                }
                inner.live = Some(Arc::new(PortalLive {
                    session,
                    core,
                    epoch: inner.epoch,
                    paint,
                    ei,
                }));
            }
            Err(e) => {
                eprintln!("[HostCapture] portal session failed: {e}");
                inner.dead = Some(e);
            }
        }
        ctl.changed.notify_all();
    }
}

/// Ask the portal for an EIS socket and bring an injector up on it, or `None` when the backend
/// refuses `ConnectToEIS` (leaving the `Notify*` path live) or the EIS handshake binds nothing.
fn open_eis(session: &PortalSession) -> Option<EiInjector> {
    let fd = match session.connect_to_eis() {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!(
                "[HostCapture] portal offers no EIS socket ({e}); input goes through the portal."
            );
            return None;
        }
    };
    match EiInjector::spawn(fd) {
        Ok(ei) => Some(ei),
        Err(e) => {
            eprintln!(
                "[HostCapture] EIS injector unavailable ({e}); input goes through the portal."
            );
            None
        }
    }
}

/// The capture thread of a portal-fed output: it holds the PipeWire stream of the output's
/// portal stream for as long as a Start aims at it, re-offering the stream's configuration
/// on every change, returning released buffers, and following the portal session across
/// reopenings. Frames themselves are published by the stream's own thread.
fn portal_capture_loop(
    index: usize,
    ctl: Arc<PortalCtl>,
    from_main: Receiver<ToHost>,
    sink: FrameSink,
    cursor_tx: std::sync::mpsc::Sender<CursorJob>,
    dma_formats: Vec<(u32, Vec<u64>)>,
    geoms: Vec<OutputGeom>,
) -> Result<(), String> {
    let mut want: Option<Want> = None;
    let mut stream: Option<(PwStream, u64)> = None;
    let mut warned_unshared = false;
    loop {
        let first = match from_main.recv_timeout(Duration::from_millis(100)) {
            Ok(m) => Some(m),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        };
        let mut pending: Vec<ToHost> = first.into_iter().collect();
        loop {
            match from_main.try_recv() {
                Ok(m) => pending.push(m),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Ok(()),
            }
        }
        for msg in pending {
            match msg {
                ToHost::Release { generation, slot } => {
                    if let Some((s, _)) = &stream {
                        s.release(generation, slot);
                    }
                }
                ToHost::Idle => {
                    want = None;
                    if let Some((s, _)) = &stream {
                        s.release_all();
                        s.set_active(false);
                    }
                }
                ToHost::Start {
                    width,
                    height,
                    zero_copy,
                    paint_cursor,
                    fps_milli,
                } => {
                    want = Some(Want {
                        size: (width, height),
                        zero_copy,
                        paint_cursor,
                        fps_milli,
                    });
                }
            }
        }
        let live = ctl.live();
        if let Some((_, epoch)) = &stream
            && live.as_ref().is_none_or(|l| l.epoch != *epoch)
        {
            stream = None;
        }
        let Some(w) = want else { continue };
        let Some(live) = live else { continue };
        let Some(core) = live.core.as_ref() else {
            continue;
        };
        let cfg = StreamConfig {
            zero_copy: w.zero_copy && !dma_formats.is_empty(),
            dma_formats: dma_formats.clone(),
            fps_milli: w.fps_milli,
            expect: w.size,
            layout: geoms.get(index).map_or((0, 0, 0, 0), |g| {
                (g.pos.0, g.pos.1, g.logical.0, g.logical.1)
            }),
        };
        match &stream {
            Some((s, _)) => {
                s.reconfigure(cfg);
                s.set_active(true);
                if s.errored() {
                    ctl.mark_dead(format!("output {index}: PipeWire stream failed"));
                    return Err("PipeWire stream failed".into());
                }
            }
            None => {
                let Some(si) = assign_streams(&live.session.streams, &geoms)
                    .get(index)
                    .copied()
                    .flatten()
                else {
                    if !warned_unshared {
                        warned_unshared = true;
                        eprintln!(
                            "[HostCapture] output {index}: the portal shares no stream for it; nothing captured."
                        );
                    }
                    continue;
                };
                match PwStream::connect(
                    core,
                    live.session.streams[si].node_id,
                    sink.clone(),
                    cursor_tx.clone(),
                    cfg,
                ) {
                    Ok(s) => stream = Some((s, live.epoch)),
                    Err(e) => {
                        ctl.mark_dead(format!("output {index}: {e}"));
                        return Err(e);
                    }
                }
            }
        }
    }
}

// Dropping the session drops every channel sender and wake-pipe write end; the
// capture and control threads observe the disconnect (poll sees POLLHUP) and
// exit on their own.

// ---------------------------------------------------------------------------
// Control thread: pumps the primary connection and applies layout requests.
// ---------------------------------------------------------------------------

fn control_loop(
    conn: Connection,
    mut queue: EventQueue<CtrlState>,
    mut state: CtrlState,
    ctrl_rx: Receiver<CtrlMsg>,
    wake_rd: OwnedFd,
    ledger: Arc<Mutex<LayoutLedger>>,
) {
    let qh = queue.handle();
    loop {
        let mut pending: Option<(u64, Vec<LayoutSlot>)> = None;
        loop {
            match ctrl_rx.try_recv() {
                Ok(CtrlMsg::Apply { epoch, slots }) => pending = Some((epoch, slots)),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
        if let Some((epoch, slots)) = pending {
            let realized = apply_layout(&conn, &mut queue, &mut state, &qh, &slots);
            ledger.lock().unwrap().decide(epoch, realized);
        }
        if queue.dispatch_pending(&mut state).is_err() {
            return;
        }
        let _ = queue.flush();
        if let Some(guard) = conn.prepare_read() {
            match wait_readable2(
                guard.connection_fd().as_raw_fd(),
                wake_rd.as_raw_fd(),
                Some(Duration::from_secs(1)),
            ) {
                Ok((wl, wake)) => {
                    if wake {
                        drain_pipe(wake_rd.as_raw_fd());
                    }
                    if wl {
                        let _ = guard.read();
                    } else {
                        drop(guard);
                    }
                }
                Err(_) => return,
            }
        }
    }
}

/// Ask the host (wlr-output-management) to give every active output its wanted mode and
/// layout position in one atomic configuration. Retries across a `canceled` (stale serial).
/// Returns whether the host said it applied the layout: `false` for a refusal, no answer by
/// `LAYOUT_DEADLINE`, or no manager at all (KWin offers only its own kde_output_management
/// protocol). Either answer settles the request, and the session owner then re-sizes its
/// capture to the mode the host announces, so capture follows the host instead of gating
/// forever on a size it will never produce.
fn apply_layout(
    conn: &Connection,
    queue: &mut EventQueue<CtrlState>,
    state: &mut CtrlState,
    qh: &QueueHandle<CtrlState>,
    slots: &[LayoutSlot],
) -> bool {
    let Some(mgr) = state.output_mgr.clone() else {
        eprintln!(
            "[HostCapture] host lacks zwlr_output_manager_v1; capture follows the host's own size."
        );
        return false;
    };
    let deadline = Instant::now() + LAYOUT_DEADLINE;
    for _ in 0..3 {
        while state.om_serial.is_none() && Instant::now() < deadline {
            if !pump_ctrl(conn, queue, state) {
                return false;
            }
        }
        let Some(serial) = state.om_serial else {
            eprintln!("[HostCapture] output-management serial never arrived; resize skipped.");
            return false;
        };
        state.cfg_result = None;
        state.cfg_canceled = false;
        let cfg = mgr.create_configuration(serial, qh, ());
        let mut any = false;
        for (i, slot) in slots.iter().enumerate() {
            if !slot.active {
                continue;
            }
            let want_name = state
                .order
                .get(i)
                .and_then(|&oi| state.outputs.get(oi))
                .and_then(|(_, n)| n.clone());
            let head = match want_name
                .as_ref()
                .and_then(|n| state.heads.iter().find(|(_, hn)| hn.as_ref() == Some(n)))
                .or_else(|| state.heads.get(i))
            {
                Some((h, _)) => h.clone(),
                None => {
                    eprintln!(
                        "[HostCapture] no output-management head for output {i}; not resized."
                    );
                    continue;
                }
            };
            let cfg_head = cfg.enable_head(&head, qh, ());
            cfg_head.set_custom_mode(slot.want.0, slot.want.1, 0);
            cfg_head.set_position(slot.pos.0, slot.pos.1);
            any = true;
        }
        if !any {
            cfg.destroy();
            return false;
        }
        cfg.apply();
        let _ = queue.flush();
        while state.cfg_result.is_none() && !state.cfg_canceled && Instant::now() < deadline {
            if !pump_ctrl(conn, queue, state) {
                cfg.destroy();
                return false;
            }
        }
        cfg.destroy();
        if state.cfg_canceled {
            // Stale serial: the compositor re-announces its state with a fresh one.
            state.om_serial = None;
            continue;
        }
        if state.cfg_result != Some(true) {
            eprintln!(
                "[HostCapture] host refused the layout; capture follows the host's own size."
            );
            return false;
        }
        return true;
    }
    false
}

/// One bounded dispatch step for the control connection (false = connection died).
fn pump_ctrl(conn: &Connection, queue: &mut EventQueue<CtrlState>, state: &mut CtrlState) -> bool {
    if queue.dispatch_pending(state).is_err() {
        return false;
    }
    let _ = queue.flush();
    let Some(guard) = conn.prepare_read() else {
        return true;
    };
    match crate::wayland::wlclient::wait_readable(
        guard.connection_fd().as_raw_fd(),
        Duration::from_millis(200),
    ) {
        Ok(readable) => {
            if readable {
                let _ = guard.read();
            } else {
                drop(guard);
            }
            true
        }
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// Capture threads: one connection + screencopy loop per host output.
// ---------------------------------------------------------------------------

fn fourcc_to_gbm(fourcc: u32) -> GbmFormat {
    // XR24 / AR24; anything else falls back to ARGB (the encoder reads BGRA bytes
    // and ignores alpha).
    match fourcc {
        0x34325258 => GbmFormat::Xrgb8888,
        _ => GbmFormat::Argb8888,
    }
}

/// A dmabuf-backed capture slot: a GBM buffer object from pixelflux's render node,
/// exported and wrapped in a host `wl_buffer`. `modifiers` narrows the allocation to
/// what the host advertised; empty leaves the driver's choice, which the host learns
/// through the params anyway. `Ok(None)` means the host refused the import (wrong
/// device, unsupported modifier): the caller degrades to shm slots instead of dying
/// to the protocol error `create_immed` would raise.
#[allow(clippy::too_many_arguments)]
fn alloc_gpu_slot(
    conn: &Connection,
    queue: &mut EventQueue<CaptureState>,
    state: &mut CaptureState,
    wake: RawFd,
    dev: &GbmDevice<File>,
    dmabuf_global: &ZwpLinuxDmabufV1,
    fourcc: u32,
    w: i32,
    h: i32,
    modifiers: &[u64],
) -> Result<Option<SlotBuffer>, String> {
    let qh = queue.handle();
    let format = fourcc_to_gbm(fourcc);
    let bo = if modifiers.is_empty() {
        crate::alloc_render_target(dev, w as u32, h as u32, format)
    } else {
        // Without the flags argument, which implies the rendering use asked for either
        // way: the entry point taking flags is Mesa 21.1 and later, and an extension
        // referencing it does not load against an older libgbm at all.
        dev.create_buffer_object_with_modifiers::<()>(
            w as u32,
            h as u32,
            format,
            modifiers.iter().map(|&m| gbm::Modifier::from(m)),
        )
        .or_else(|_| crate::alloc_render_target(dev, w as u32, h as u32, format))
    }
    .map_err(|e| format!("GBM allocation {w}x{h}: {e:?}"))?;
    let dmabuf = crate::create_dmabuf_from_bo(&bo);
    let params = dmabuf_global.create_params(&qh, ());
    let modifier: u64 = dmabuf.format().modifier.into();
    for (i, handle) in dmabuf.handles().enumerate() {
        params.add(
            handle,
            i as u32,
            dmabuf.offsets().nth(i).unwrap_or(0),
            dmabuf.strides().nth(i).unwrap_or(0),
            (modifier >> 32) as u32,
            (modifier & 0xffff_ffff) as u32,
        );
    }
    state.params_created = None;
    state.params_failed = false;
    params.create(w, h, fourcc, zwp_linux_buffer_params_v1::Flags::empty());
    loop {
        match pump_until(
            conn,
            queue,
            state,
            wake,
            Some(Duration::from_secs(2)),
            |s| s.params_created.is_some() || s.params_failed,
        )? {
            Pump::Done => break,
            Pump::Control => continue,
            Pump::Timeout => {
                params.destroy();
                return Err("host answered neither created nor failed for the dmabuf".into());
            }
        }
    }
    params.destroy();
    match state.params_created.take() {
        Some(wl) => Ok(Some(SlotBuffer::Gpu {
            _bo: bo,
            dmabuf,
            wl,
        })),
        None => Ok(None),
    }
}

/// An shm-backed capture slot in a memfd pool the CPU consumer reads in place.
fn alloc_cpu_slot(
    shm: &wl_shm::WlShm,
    qh: &QueueHandle<CaptureState>,
    format: u32,
    w: i32,
    h: i32,
    stride: i32,
) -> Result<SlotBuffer, String> {
    let size = (stride * h) as usize;
    let fd = memfd_with(&vec![0u8; size])?;
    let pool = shm.create_pool(fd.as_fd(), size as i32, qh, ());
    let wl = pool.create_buffer(
        0,
        w,
        h,
        stride,
        WEnum::<wl_shm::Format>::from(format)
            .into_result()
            .unwrap_or(wl_shm::Format::Xrgb8888),
        qh,
        (),
    );
    let file = File::from(fd);
    let map = unsafe { memmap2::MmapMut::map_mut(&file) }.map_err(|e| format!("shm map: {e}"))?;
    Ok(SlotBuffer::Cpu {
        _pool: pool,
        map: Arc::new(map),
        stride,
        format,
        wl,
    })
}

/// What a control-channel drain decided while a capture wait was in progress.
enum Ctl {
    None,
    Renegotiate,
    Idle,
    Dead,
}

enum Pump {
    Done,
    Control,
    Timeout,
}

/// Dispatch events until `done(state)` holds, waiting on the Wayland socket and
/// the wake pipe together so control messages interrupt any capture wait. The
/// predicate is re-checked after every dispatch, so a wait whose condition was
/// satisfied by already-queued events returns without ever blocking.
fn pump_until(
    conn: &Connection,
    queue: &mut EventQueue<CaptureState>,
    state: &mut CaptureState,
    wake_rd: RawFd,
    timeout: Option<Duration>,
    done: impl Fn(&CaptureState) -> bool,
) -> Result<Pump, String> {
    let deadline = timeout.map(|t| Instant::now() + t);
    loop {
        queue
            .dispatch_pending(state)
            .map_err(|e| format!("dispatch: {e}"))?;
        if done(state) {
            return Ok(Pump::Done);
        }
        queue.flush().map_err(|e| format!("flush: {e}"))?;
        let remaining = match deadline {
            Some(d) => match d.checked_duration_since(Instant::now()) {
                Some(r) => Some(r),
                None => return Ok(Pump::Timeout),
            },
            None => None,
        };
        let Some(guard) = conn.prepare_read() else {
            continue;
        };
        let (wl, wake) = wait_readable2(guard.connection_fd().as_raw_fd(), wake_rd, remaining)?;
        if wake {
            drop(guard);
            drain_pipe(wake_rd);
            return Ok(Pump::Control);
        }
        if wl {
            let _ = guard.read();
        } else {
            drop(guard);
            if deadline.is_some_and(|d| Instant::now() >= d) {
                return Ok(Pump::Timeout);
            }
        }
    }
}

fn capture_loop(
    display: &str,
    index: usize,
    expect_name: Option<String>,
    gbm_path: Option<std::path::PathBuf>,
    from_main: Receiver<ToHost>,
    wake_rd: OwnedFd,
    frames: FrameSink,
) -> Result<(), String> {
    let path = socket_path(display).ok_or("XDG_RUNTIME_DIR is unset")?;
    let stream = UnixStream::connect(&path).map_err(|e| format!("connect {path}: {e}"))?;
    let conn = Connection::from_socket(stream).map_err(|e| format!("wayland setup: {e}"))?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut state = CaptureState::default();
    bounded_roundtrip(&conn, &mut queue, &mut state)?;
    // Names (wl_output v4) arrive after the binds from the first round-trip.
    bounded_roundtrip(&conn, &mut queue, &mut state)?;

    // Select this thread's output by the name the control connection assigned
    // it; ordering fallback only when the host names nothing.
    let by_name = expect_name.as_ref().and_then(|expect| {
        state
            .outputs
            .iter()
            .find(|(_, n)| n.as_ref() == Some(expect))
            .cloned()
    });
    let (output, _name) = match by_name {
        Some(o) => o,
        None => {
            let mut order: Vec<usize> = (0..state.outputs.len()).collect();
            order.sort_by_key(|&i| output_order_key(state.outputs[i].1.as_ref(), i));
            let oi = *order
                .get(index)
                .ok_or_else(|| format!("host has no wl_output {index}"))?;
            state.outputs[oi].clone()
        }
    };
    let gbm = gbm_path
        .as_ref()
        .and_then(|p| File::options().read(true).write(true).open(p).ok())
        .and_then(|f| GbmDevice::new(f).ok());
    let wake = wake_rd.as_raw_fd();

    // Hosts speaking ext-image-copy-capture (wlroots 0.19+, KWin 6.2+, cosmic) get a
    // persistent session with compositor-side damage gating; everything else takes
    // wlr-screencopy v3 below, unchanged. PIXELFLUX_HOST_CAPTURE=zwlr forces the
    // fallback for triage.
    // The current aim outlives the ext attempt: a Start consumed while that session was
    // being opened still drives the wlr-screencopy fallback.
    let mut want: Option<Want> = None;
    let force = std::env::var("PIXELFLUX_HOST_CAPTURE").unwrap_or_default();
    if force != "zwlr" && state.ext_capture.is_some() && state.ext_source_mgr.is_some() {
        match capture_loop_ext(
            &conn,
            &mut queue,
            &mut state,
            &output,
            gbm.as_ref(),
            wake,
            index,
            &from_main,
            &frames,
            &mut want,
        ) {
            ExtOutcome::Finished => return Ok(()),
            ExtOutcome::Unavailable(e) => {
                eprintln!(
                    "[HostCapture] output {index}: ext capture unavailable ({e}); using wlr-screencopy."
                );
            }
        }
    }

    let screencopy = state
        .screencopy
        .clone()
        .ok_or("no zwlr_screencopy_manager_v1")?;

    let mut slots: Vec<Option<SlotBuffer>> = (0..SLOTS).map(|_| None).collect();
    let mut free: Vec<usize> = (0..SLOTS).collect();
    let mut generation: u64 = 0;
    let mut announced: Option<(i32, i32)> = None;
    let mut warned_mismatch = false;
    let mut consecutive_failures = 0u32;
    // Buffer type the currently allocated slots hold; None while none exist.
    let mut slot_zero_copy: Option<bool> = None;
    let mut gpu_refused = false;

    'main: loop {
        // Drain control; block while idle or out of slots.
        loop {
            let blocking = want.is_none() || free.is_empty();
            let msg = if blocking {
                match from_main.recv() {
                    Ok(m) => m,
                    Err(_) => return Ok(()),
                }
            } else {
                match from_main.try_recv() {
                    Ok(m) => m,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return Ok(()),
                }
            };
            match msg {
                ToHost::Release {
                    generation: g,
                    slot,
                } => {
                    if g == generation {
                        free.push(slot);
                    }
                }
                ToHost::Idle => want = None,
                ToHost::Start {
                    width,
                    height,
                    zero_copy,
                    paint_cursor,
                    fps_milli,
                } => {
                    let next = Want {
                        size: (width, height),
                        zero_copy,
                        paint_cursor,
                        fps_milli,
                    };
                    if want != Some(next) {
                        warned_mismatch = false;
                    }
                    want = Some(next);
                }
            }
        }
        let (want_w, want_h, want_zero_copy, want_paint) = match want {
            Some(w) => (w.size.0, w.size.1, w.zero_copy, w.paint_cursor),
            None => continue,
        };

        // One frame: negotiate, attach our buffer, wait for damage. Control
        // messages (resize, idle, teardown) interrupt any of the waits.
        state.reset_frame();
        let frame = screencopy.capture_output(i32::from(want_paint), &output, &qh, ());
        loop {
            match pump_until(&conn, &mut queue, &mut state, wake, None, |s| {
                s.buffer_done || s.failed
            })? {
                Pump::Done => break,
                Pump::Control => match drain_ctl(&from_main, generation, &mut free, &mut want) {
                    Ctl::None => {}
                    Ctl::Renegotiate | Ctl::Idle => {
                        frame.destroy();
                        continue 'main;
                    }
                    Ctl::Dead => {
                        frame.destroy();
                        return Ok(());
                    }
                },
                Pump::Timeout => {}
            }
        }
        if state.failed {
            frame.destroy();
            consecutive_failures += 1;
            if consecutive_failures == 3 {
                eprintln!(
                    "[HostCapture] output {index}: repeated screencopy failures; is the output alive?"
                );
            }
            let _ = pump_until(
                &conn,
                &mut queue,
                &mut state,
                wake,
                Some(Duration::from_millis(100)),
                |_| false,
            );
            continue;
        }

        let (fw, fh) = state
            .announce_dmabuf
            .map(|(_, w, h)| (w, h))
            .or(state.announce_shm.map(|(_, w, h, _)| (w, h)))
            .ok_or("screencopy announced no buffer type")?;
        let mut renegotiate = false;
        if announced != Some((fw, fh)) {
            announced = Some((fw, fh));
            eprintln!(
                "[HostCapture] output {index} negotiated: {fw}x{fh} gbm={} dmabuf_global={} dmabuf_announce={:?} shm={:?}",
                gbm.is_some(),
                state.dmabuf.is_some(),
                state.announce_dmabuf,
                state.announce_shm,
            );
            renegotiate = true;
        }
        if slot_zero_copy.is_some_and(|z| z != want_zero_copy) {
            // The consumer switched between importing dmabufs and reading pixels (an
            // encoder demoted, or a restart with different encode settings), so the
            // allocated buffers are the wrong kind.
            renegotiate = true;
        }
        if renegotiate {
            // Drop the stale buffers. The generation bump makes any release still in
            // flight for them a no-op.
            for s in slots.iter_mut() {
                *s = None;
            }
            free = (0..SLOTS).collect();
            slot_zero_copy = None;
            gpu_refused = false;
            generation += 1;
        }
        if (fw, fh) != (want_w, want_h) {
            // The encoder is configured for (want_w, want_h): hold frames until
            // the host applies the resize rather than encode mismatched buffers.
            frame.destroy();
            if !warned_mismatch {
                warned_mismatch = true;
                eprintln!(
                    "[HostCapture] output {index}: waiting for host {want_w}x{want_h} (currently {fw}x{fh})."
                );
            }
            let _ = pump_until(
                &conn,
                &mut queue,
                &mut state,
                wake,
                Some(Duration::from_millis(150)),
                |_| false,
            );
            continue;
        }
        warned_mismatch = false;

        let slot_idx = *free.last().unwrap();
        if slots[slot_idx].is_none() {
            // Only a consumer that imports dmabufs gets GPU slots; anything reading the
            // pixels on the CPU takes the shm path even where the host offers dmabuf.
            slot_zero_copy = Some(want_zero_copy);
            let mut built: Option<SlotBuffer> = None;
            if want_zero_copy && !gpu_refused {
                let dmabuf_global = state.dmabuf.clone();
                if let (Some(dev), Some(dmabuf_global), Some((fourcc, w, h))) =
                    (gbm.as_ref(), dmabuf_global.as_ref(), state.announce_dmabuf)
                {
                    match alloc_gpu_slot(
                        &conn,
                        &mut queue,
                        &mut state,
                        wake,
                        dev,
                        dmabuf_global,
                        fourcc,
                        w,
                        h,
                        &[],
                    )? {
                        Some(slot) => built = Some(slot),
                        None => {
                            gpu_refused = true;
                            eprintln!(
                                "[HostCapture] output {index}: host refused the dmabuf import; capturing via shm."
                            );
                        }
                    }
                }
            }
            slots[slot_idx] = Some(match built {
                Some(slot) => slot,
                None => {
                    let (format, w, h, stride) =
                        state.announce_shm.ok_or("no shm fallback announced")?;
                    let shm = state.shm.clone().ok_or("host compositor lacks wl_shm")?;
                    alloc_cpu_slot(&shm, &qh, format, w, h, stride)?
                }
            });
        }
        free.pop();

        {
            let slot = slots[slot_idx].as_ref().unwrap();
            let wl = match slot {
                SlotBuffer::Gpu { wl, .. } => wl,
                SlotBuffer::Cpu { wl, .. } => wl,
            };
            frame.copy_with_damage(wl);
        }
        queue.flush().map_err(|e| format!("flush: {e}"))?;
        let mut aborted = false;
        loop {
            match pump_until(&conn, &mut queue, &mut state, wake, None, |s| {
                s.ready || s.failed
            })? {
                Pump::Done => break,
                Pump::Control => match drain_ctl(&from_main, generation, &mut free, &mut want) {
                    Ctl::None => {}
                    Ctl::Renegotiate | Ctl::Idle => {
                        aborted = true;
                        break;
                    }
                    Ctl::Dead => {
                        frame.destroy();
                        return Ok(());
                    }
                },
                Pump::Timeout => {}
            }
        }
        frame.destroy();
        if aborted {
            free.push(slot_idx);
            continue 'main;
        }
        if state.failed {
            free.push(slot_idx);
            consecutive_failures += 1;
            if consecutive_failures == 3 {
                eprintln!(
                    "[HostCapture] output {index}: repeated screencopy failures; is the output alive?"
                );
            }
            let _ = pump_until(
                &conn,
                &mut queue,
                &mut state,
                wake,
                Some(Duration::from_millis(50)),
                |_| false,
            );
            continue;
        }
        consecutive_failures = 0;

        let damage = std::mem::take(&mut state.damage);
        let out = match slots[slot_idx].as_ref().unwrap() {
            SlotBuffer::Gpu { dmabuf, .. } => HostFrame {
                generation,
                slot: slot_idx,
                dmabuf: Some(dmabuf.clone()),
                cpu: None,
                width: fw,
                height: fh,
                damage,
                stamp_ns: now_ns(),
            },
            SlotBuffer::Cpu {
                map,
                stride,
                format,
                ..
            } => HostFrame {
                generation,
                slot: slot_idx,
                dmabuf: None,
                cpu: Some(HostCpuFrame {
                    map: map.clone() as Arc<dyn AsRef<[u8]> + Send + Sync>,
                    offset: 0,
                    stride: *stride as usize,
                    format: *format,
                }),
                width: fw,
                height: fh,
                damage,
                stamp_ns: now_ns(),
            },
        };
        if !frames.send(out) {
            return Ok(());
        }
    }
}

/// How the ext capture loop ended: `Finished` is a normal teardown (the thread is
/// done); `Unavailable` means it could not get going, so wlr-screencopy should run.
enum ExtOutcome {
    Finished,
    Unavailable(String),
}

/// ext-image-copy-capture capture loop: one persistent session per output. The
/// compositor holds each capture until the content actually changes, so a static
/// screen costs nothing — the property `copy_with_damage` provides on the wlr path.
/// Frames land in the same slot machinery and `HostFrame` shape as that path.
#[allow(clippy::too_many_arguments)]
fn capture_loop_ext(
    conn: &Connection,
    queue: &mut EventQueue<CaptureState>,
    state: &mut CaptureState,
    output: &wl_output::WlOutput,
    gbm: Option<&GbmDevice<File>>,
    wake: RawFd,
    index: usize,
    from_main: &Receiver<ToHost>,
    frames: &FrameSink,
    want: &mut Option<Want>,
) -> ExtOutcome {
    let qh = queue.handle();
    let mut slots: Vec<Option<SlotBuffer>> = (0..SLOTS).map(|_| None).collect();
    let mut free: Vec<usize> = (0..SLOTS).collect();
    let mut generation: u64 = 0;
    let mut slot_zero_copy: Option<bool> = None;
    let mut gpu_refused = false;
    let mut seen_serial: u64 = 0;
    let mut warned_mismatch = false;
    let mut consecutive_failures = 0u32;

    let teardown = |session: &ExtImageCopyCaptureSessionV1, source: &ExtImageCaptureSourceV1| {
        session.destroy();
        source.destroy();
    };

    // The session opens with the cursor option of the first Start, which may already be
    // queued; before any Start the consumer's default (drawing the cursor itself) applies.
    let mut session_paints = false;
    let (mut source, mut session) = match open_ext_session(
        conn,
        queue,
        state,
        output,
        wake,
        index,
        from_main,
        session_paints,
        generation,
        &mut free,
        want,
    ) {
        Ok(opened) => opened,
        Err(outcome) => return outcome,
    };

    'main: loop {
        // Drain control; block while idle or out of slots.
        loop {
            let blocking = want.is_none() || free.is_empty();
            let msg = if blocking {
                match from_main.recv() {
                    Ok(m) => m,
                    Err(_) => {
                        teardown(&session, &source);
                        return ExtOutcome::Finished;
                    }
                }
            } else {
                match from_main.try_recv() {
                    Ok(m) => m,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        teardown(&session, &source);
                        return ExtOutcome::Finished;
                    }
                }
            };
            match msg {
                ToHost::Release {
                    generation: g,
                    slot,
                } => {
                    if g == generation {
                        free.push(slot);
                    }
                }
                ToHost::Idle => *want = None,
                ToHost::Start {
                    width,
                    height,
                    zero_copy,
                    paint_cursor,
                    fps_milli,
                } => {
                    let next = Want {
                        size: (width, height),
                        zero_copy,
                        paint_cursor,
                        fps_milli,
                    };
                    if *want != Some(next) {
                        warned_mismatch = false;
                    }
                    *want = Some(next);
                }
            }
        }
        let (want_w, want_h, want_zero_copy, want_paint) = match *want {
            Some(w) => (w.size.0, w.size.1, w.zero_copy, w.paint_cursor),
            None => continue,
        };
        if state.ext_stopped {
            // The output is going away; nothing else on this connection will revive it.
            teardown(&session, &source);
            return ExtOutcome::Finished;
        }
        if want_paint != session_paints {
            // The cursor option is fixed per session: reopen with the wanted one. Its
            // constraints arrive under a new serial, which reallocates the slots below.
            teardown(&session, &source);
            (source, session) = match open_ext_session(
                conn, queue, state, output, wake, index, from_main, want_paint, generation,
                &mut free, want,
            ) {
                Ok(opened) => opened,
                Err(outcome) => return outcome,
            };
            session_paints = want_paint;
            continue;
        }

        // Fresh constraints obsolete every allocated buffer.
        if seen_serial != state.ext_serial || slot_zero_copy.is_some_and(|z| z != want_zero_copy) {
            seen_serial = state.ext_serial;
            for s in slots.iter_mut() {
                *s = None;
            }
            free = (0..SLOTS).collect();
            slot_zero_copy = None;
            gpu_refused = false;
            generation += 1;
        }

        let Some((cw, ch)) = state.ext_size else {
            teardown(&session, &source);
            return ExtOutcome::Unavailable("constraints carried no size".into());
        };
        if (cw, ch) != (want_w, want_h) {
            // The encoder is configured for (want_w, want_h): hold frames until the
            // host applies the resize rather than capture mismatched buffers.
            if !warned_mismatch {
                warned_mismatch = true;
                eprintln!(
                    "[HostCapture] output {index}: waiting for host {want_w}x{want_h} (currently {cw}x{ch})."
                );
            }
            let _ = pump_until(
                conn,
                queue,
                state,
                wake,
                Some(Duration::from_millis(150)),
                |s| s.ext_serial != seen_serial,
            );
            continue;
        }
        warned_mismatch = false;

        let slot_idx = *free.last().unwrap();
        if slots[slot_idx].is_none() {
            // Only a consumer that imports dmabufs gets GPU slots; anything reading
            // the pixels on the CPU takes the shm path even where the host offers
            // dmabuf. XR24/AR24 keep the bytes BGRA, which is what every consumer
            // downstream assumes.
            let dma_choice = [0x3432_5258u32, 0x3432_5241]
                .iter()
                .find_map(|f| state.ext_dma_formats.iter().find(|(code, _)| code == f))
                .cloned();
            slot_zero_copy = Some(want_zero_copy);
            let mut slot: Option<SlotBuffer> = None;
            if want_zero_copy && !gpu_refused {
                let dmabuf_global = state.dmabuf.clone();
                if let (Some(dev), Some(dmabuf_global), Some((fourcc, modifiers))) =
                    (gbm, dmabuf_global.as_ref(), dma_choice)
                {
                    match alloc_gpu_slot(
                        conn,
                        queue,
                        state,
                        wake,
                        dev,
                        dmabuf_global,
                        fourcc,
                        cw,
                        ch,
                        &modifiers,
                    ) {
                        Ok(Some(built)) => slot = Some(built),
                        Ok(None) => {
                            gpu_refused = true;
                            eprintln!(
                                "[HostCapture] output {index}: host refused the dmabuf import; capturing via shm."
                            );
                        }
                        Err(e) => {
                            teardown(&session, &source);
                            return ExtOutcome::Unavailable(e);
                        }
                    }
                }
            }
            let built = match slot {
                Some(slot) => Ok(slot),
                None => {
                    let format = [1u32, 0]
                        .iter()
                        .find(|f| state.ext_shm_formats.contains(f))
                        .copied()
                        .or_else(|| state.ext_shm_formats.first().copied());
                    match (state.shm.clone(), format) {
                        (Some(shm), Some(format)) => {
                            alloc_cpu_slot(&shm, &qh, format, cw, ch, cw * 4)
                        }
                        _ => Err("host offers no usable shm format".into()),
                    }
                }
            };
            match built {
                Ok(slot) => slots[slot_idx] = Some(slot),
                Err(e) => {
                    teardown(&session, &source);
                    return ExtOutcome::Unavailable(e);
                }
            }
        }
        free.pop();

        state.reset_frame();
        let frame = {
            let slot = slots[slot_idx].as_ref().unwrap();
            let wl = match slot {
                SlotBuffer::Gpu { wl, .. } => wl,
                SlotBuffer::Cpu { wl, .. } => wl,
            };
            let frame = session.create_frame(&qh, ());
            frame.attach_buffer(wl);
            // Slots rotate, so the attached buffer holds a frame from SLOTS captures
            // ago: everything in it is stale from the host's point of view.
            frame.damage_buffer(0, 0, cw, ch);
            frame.capture();
            frame
        };
        if let Err(e) = queue.flush() {
            frame.destroy();
            teardown(&session, &source);
            return ExtOutcome::Unavailable(format!("flush: {e}"));
        }

        let mut aborted = false;
        loop {
            match pump_until(conn, queue, state, wake, None, |s| {
                s.ready || s.failed || s.ext_stopped || s.ext_serial != seen_serial
            }) {
                Ok(Pump::Done) => break,
                Ok(Pump::Control) => match drain_ctl(from_main, generation, &mut free, want) {
                    Ctl::None => {}
                    Ctl::Renegotiate | Ctl::Idle => {
                        aborted = true;
                        break;
                    }
                    Ctl::Dead => {
                        frame.destroy();
                        teardown(&session, &source);
                        return ExtOutcome::Finished;
                    }
                },
                Ok(Pump::Timeout) => {}
                Err(e) => {
                    frame.destroy();
                    teardown(&session, &source);
                    return ExtOutcome::Unavailable(e);
                }
            }
        }
        frame.destroy();
        if aborted || state.ext_serial != seen_serial {
            free.push(slot_idx);
            continue 'main;
        }
        if state.ext_stopped {
            teardown(&session, &source);
            return ExtOutcome::Finished;
        }
        if state.failed {
            free.push(slot_idx);
            if state.ext_fail_reason
                == Some(ext_image_copy_capture_frame_v1::FailureReason::BufferConstraints)
            {
                // Constraints changed under us; the serial check above rebuilds the
                // buffers on the next pass.
                continue;
            }
            consecutive_failures += 1;
            if consecutive_failures == 3 {
                eprintln!(
                    "[HostCapture] output {index}: repeated capture failures; is the output alive?"
                );
            }
            let _ = pump_until(
                conn,
                queue,
                state,
                wake,
                Some(Duration::from_millis(50)),
                |_| false,
            );
            continue;
        }
        consecutive_failures = 0;

        let damage = std::mem::take(&mut state.damage);
        let out = match slots[slot_idx].as_ref().unwrap() {
            SlotBuffer::Gpu { dmabuf, .. } => HostFrame {
                generation,
                slot: slot_idx,
                dmabuf: Some(dmabuf.clone()),
                cpu: None,
                width: cw,
                height: ch,
                damage,
                stamp_ns: now_ns(),
            },
            SlotBuffer::Cpu {
                map,
                stride,
                format,
                ..
            } => HostFrame {
                generation,
                slot: slot_idx,
                dmabuf: None,
                cpu: Some(HostCpuFrame {
                    map: map.clone() as Arc<dyn AsRef<[u8]> + Send + Sync>,
                    offset: 0,
                    stride: *stride as usize,
                    format: *format,
                }),
                width: cw,
                height: ch,
                damage,
                stamp_ns: now_ns(),
            },
        };
        if !frames.send(out) {
            teardown(&session, &source);
            return ExtOutcome::Finished;
        }
    }
}

/// Open an ext capture session on `output`, with or without the host painting its cursor
/// into the frames, and wait for its first buffer constraints: the point that decides
/// whether this protocol works here at all. A session's cursor option is fixed at
/// creation, so a toggle closes the session and opens another through here.
#[allow(clippy::too_many_arguments)]
fn open_ext_session(
    conn: &Connection,
    queue: &mut EventQueue<CaptureState>,
    state: &mut CaptureState,
    output: &wl_output::WlOutput,
    wake: RawFd,
    index: usize,
    from_main: &Receiver<ToHost>,
    paint_cursor: bool,
    generation: u64,
    free: &mut Vec<usize>,
    want: &mut Option<Want>,
) -> Result<(ExtImageCaptureSourceV1, ExtImageCopyCaptureSessionV1), ExtOutcome> {
    let qh = queue.handle();
    let src_mgr = state.ext_source_mgr.clone().expect("checked by caller");
    let mgr = state.ext_capture.clone().expect("checked by caller");
    let source = src_mgr.create_source(output, &qh, ());
    let options = if paint_cursor {
        ext_image_copy_capture_manager_v1::Options::PaintCursors
    } else {
        ext_image_copy_capture_manager_v1::Options::empty()
    };
    let session = mgr.create_session(&source, options, &qh, ());
    let teardown = |session: &ExtImageCopyCaptureSessionV1, source: &ExtImageCaptureSourceV1| {
        session.destroy();
        source.destroy();
    };
    // A previous session's stop or half-streamed constraints must not pass for this one's.
    state.ext_stopped = false;
    state.ext_pending_size = None;
    state.ext_pending_shm.clear();
    state.ext_pending_dma.clear();
    let before = state.ext_serial;
    loop {
        match pump_until(
            conn,
            queue,
            state,
            wake,
            Some(Duration::from_secs(5)),
            |s| s.ext_serial > before || s.ext_stopped,
        ) {
            Ok(Pump::Done) => break,
            Ok(Pump::Control) => match drain_ctl(from_main, generation, free, want) {
                Ctl::Dead => {
                    teardown(&session, &source);
                    return Err(ExtOutcome::Finished);
                }
                _ => continue,
            },
            Ok(Pump::Timeout) => {
                teardown(&session, &source);
                return Err(ExtOutcome::Unavailable(
                    "no buffer constraints within 5s".into(),
                ));
            }
            Err(e) => {
                teardown(&session, &source);
                return Err(ExtOutcome::Unavailable(e));
            }
        }
    }
    if state.ext_stopped {
        teardown(&session, &source);
        return Err(ExtOutcome::Unavailable(
            "session stopped before constraints".into(),
        ));
    }
    eprintln!(
        "[HostCapture] output {index} ext session: {:?} dma_formats={} shm_formats={} cursor={}",
        state.ext_size,
        state.ext_dma_formats.len(),
        state.ext_shm_formats.len(),
        if paint_cursor { "painted" } else { "consumer" },
    );
    Ok((source, session))
}

/// Drain the control channel from inside a capture wait: releases are applied
/// (generation-checked), and the newest Start/Idle decides the wait's fate.
fn drain_ctl(
    rx: &Receiver<ToHost>,
    generation: u64,
    free: &mut Vec<usize>,
    want: &mut Option<Want>,
) -> Ctl {
    let mut out = Ctl::None;
    loop {
        match rx.try_recv() {
            Ok(ToHost::Release {
                generation: g,
                slot,
            }) => {
                if g == generation {
                    free.push(slot);
                }
            }
            Ok(ToHost::Start {
                width,
                height,
                zero_copy,
                paint_cursor,
                fps_milli,
            }) => {
                let next = Want {
                    size: (width, height),
                    zero_copy,
                    paint_cursor,
                    fps_milli,
                };
                if *want != Some(next) {
                    *want = Some(next);
                    out = Ctl::Renegotiate;
                }
            }
            Ok(ToHost::Idle) => {
                *want = None;
                out = Ctl::Idle;
            }
            Err(TryRecvError::Empty) => return out,
            Err(TryRecvError::Disconnected) => return Ctl::Dead,
        }
    }
}

#[cfg(test)]
mod tests {
    /// A finger's travel turns into whole notches with the fraction carried, both ways.
    #[test]
    fn a_finger_scroll_turns_into_whole_notches_with_the_rest_carried() {
        let mut carry = 0.0;
        assert_eq!(super::scroll_notches(&mut carry, 60.0), 0);
        assert_eq!(super::scroll_notches(&mut carry, 60.0), 1);
        assert!((carry - 20.0).abs() < 1e-9);
        assert_eq!(super::scroll_notches(&mut carry, -260.0), -2);
        assert!((carry + 40.0).abs() < 1e-9);
    }

    use super::*;

    fn us_host_keyboard() -> HostKeyboardState {
        let text = crate::wayland::keymap::compile_rmlvo("", "", "us", "", "").unwrap();
        let mut keyboard = HostKeyboardState::default();
        assert!(keyboard.set_keymap(&text).is_some());
        keyboard
    }

    /// Every shm layout a compositor may announce, converted to BGRA.
    #[test]
    fn shm_rows_reach_bgra_from_each_announced_layout() {
        let cases = [
            (
                wl_shm::Format::Xrgb8888 as u32,
                vec![1u8, 2, 3, 4],
                vec![1u8, 2, 3, 4],
            ),
            (
                wl_shm::Format::Xbgr8888 as u32,
                vec![1u8, 2, 3, 9],
                vec![3u8, 2, 1, 9],
            ),
            (
                wl_shm::Format::Bgr888 as u32,
                vec![1u8, 2, 3],
                vec![3u8, 2, 1, 0xff],
            ),
            (
                wl_shm::Format::Rgb888 as u32,
                vec![1u8, 2, 3],
                vec![1u8, 2, 3, 0xff],
            ),
        ];
        for (format, src, want) in cases {
            let (bpp, swap) = shm_src_layout(format);
            let mut dst = vec![0u8; 4];
            convert_shm_row(&src, &mut dst, bpp, swap);
            assert_eq!(dst, want, "format {format}");
        }
    }

    /// A row wider than one pixel converts every pixel, not just the first.
    #[test]
    fn shm_rows_convert_every_pixel_of_the_row() {
        let src: Vec<u8> = (0..12u8).collect();
        let mut dst = vec![0u8; 16];
        convert_shm_row(&src, &mut dst, 3, true);
        assert_eq!(
            dst,
            vec![2, 1, 0, 0xff, 5, 4, 3, 0xff, 8, 7, 6, 0xff, 11, 10, 9, 0xff]
        );
        let src: Vec<u8> = (0..8u8).collect();
        let mut dst = vec![0u8; 8];
        convert_shm_row(&src, &mut dst, 4, true);
        assert_eq!(dst, vec![2, 1, 0, 3, 6, 5, 4, 7]);
    }

    #[test]
    fn host_keyboard_serializes_control_modifier() {
        let mut keyboard = us_host_keyboard();
        let control_down = keyboard.update_key(37, true).unwrap();
        assert_ne!(control_down.depressed, 0);
        let control_up = keyboard.update_key(37, false).unwrap();
        assert_eq!(control_up.depressed, 0);
    }

    #[test]
    fn host_keyboard_ignores_repeated_key_down() {
        let mut keyboard = us_host_keyboard();
        keyboard.update_key(37, true).unwrap();
        assert!(keyboard.update_key(37, true).is_none());
        let control_up = keyboard.update_key(37, false).unwrap();
        assert_eq!(control_up.depressed, 0);
    }

    #[test]
    fn host_keyboard_keeps_lock_across_keymap_changes() {
        let text = crate::wayland::keymap::compile_rmlvo("", "", "us", "", "").unwrap();
        let mut keyboard = us_host_keyboard();
        keyboard.update_key(66, true).unwrap();
        let locked = keyboard.update_key(66, false).unwrap();
        assert_ne!(locked.locked, 0);
        // A keymap swap must not drop the lock; the modifiers() that follows would
        // otherwise clear CapsLock on the host.
        let after_keymap = keyboard.set_keymap(&text).unwrap();
        assert_ne!(after_keymap.locked, 0);
    }

    #[test]
    fn host_keyboard_preserves_pressed_modifiers_across_keymap_changes() {
        let text = crate::wayland::keymap::compile_rmlvo("", "", "us", "", "").unwrap();
        let mut keyboard = us_host_keyboard();
        keyboard.update_key(37, true).unwrap();
        let after_keymap = keyboard.set_keymap(&text).unwrap();
        assert_ne!(after_keymap.depressed, 0);
        let control_up = keyboard.update_key(37, false).unwrap();
        assert_eq!(control_up.depressed, 0);
    }

    #[test]
    fn host_keyboard_keeps_control_active_until_both_keys_are_released() {
        let mut keyboard = us_host_keyboard();
        keyboard.update_key(37, true).unwrap();
        keyboard.update_key(105, true).unwrap();
        let after_left_release = keyboard.update_key(37, false).unwrap();
        assert_ne!(after_left_release.depressed, 0);
        let after_right_release = keyboard.update_key(105, false).unwrap();
        assert_eq!(after_right_release.depressed, 0);
    }

    #[test]
    fn host_keyboard_ignores_repeated_lock_key_down() {
        let mut keyboard = us_host_keyboard();
        keyboard.update_key(66, true).unwrap();
        assert!(keyboard.update_key(66, true).is_none());
        let caps_lock = keyboard.update_key(66, false).unwrap();
        assert_eq!(caps_lock.depressed, 0);
        assert_ne!(caps_lock.locked, 0);
    }

    /// The shm-format table maps each announced format to the right BGRA conversion. The
    /// wl_shm definitions are little-endian, so `Bgr888` (`[23:0] B:G:R`) is red-first in memory and
    /// must swap R/B, while `Rgb888` (`[23:0] R:G:B`) is already blue-first and copies straight.
    #[test]
    fn shm_layout_matches_wl_shm_definitions() {
        assert_eq!(shm_src_layout(wl_shm::Format::Bgr888 as u32), (3, true));
        assert_eq!(shm_src_layout(wl_shm::Format::Rgb888 as u32), (3, false));
        assert_eq!(shm_src_layout(wl_shm::Format::Xbgr8888 as u32), (4, true));
        assert_eq!(shm_src_layout(wl_shm::Format::Abgr8888 as u32), (4, true));
        assert_eq!(shm_src_layout(wl_shm::Format::Xrgb8888 as u32), (4, false));
        assert_eq!(shm_src_layout(wl_shm::Format::Argb8888 as u32), (4, false));
    }

    /// A known pixel round-trips to BGRA. The logical color is R=0x11, G=0x22, B=0x33; in a
    /// `Bgr888` buffer its little-endian bytes are R,G,B (0x11,0x22,0x33) and the destination must be
    /// B,G,R,A (0x33,0x22,0x11,0xff). Without the R/B swap this path produced 0x11,0x22,0x33 —
    /// red and blue transposed.
    #[test]
    fn bgr888_known_pixel_swaps_red_and_blue() {
        let (bpp, swap) = shm_src_layout(wl_shm::Format::Bgr888 as u32);
        let src = [0x11u8, 0x22, 0x33];
        let mut dst = [0u8; 4];
        convert_shm_row(&src, &mut dst, bpp, swap);
        assert_eq!(dst, [0x33, 0x22, 0x11, 0xff]);
    }

    /// The same logical color in an `Rgb888` buffer is already B,G,R in memory
    /// (0x33,0x22,0x11), so it copies straight to B,G,R,A with an opaque alpha and no swap.
    #[test]
    fn rgb888_known_pixel_copies_straight() {
        let (bpp, swap) = shm_src_layout(wl_shm::Format::Rgb888 as u32);
        let src = [0x33u8, 0x22, 0x11];
        let mut dst = [0u8; 4];
        convert_shm_row(&src, &mut dst, bpp, swap);
        assert_eq!(dst, [0x33, 0x22, 0x11, 0xff]);
    }

    /// The 32-bit `Xbgr8888` source (R,G,B,x in memory) swaps R/B and preserves the fourth
    /// byte, and `Xrgb8888` (B,G,R,x) copies straight.
    fn convert(format: wl_shm::Format, src: &[u8]) -> [u8; 4] {
        let (bpp, swap) = shm_src_layout(format as u32);
        let mut dst = [0u8; 4];
        convert_shm_row(src, &mut dst, bpp, swap);
        dst
    }

    #[test]
    fn four_byte_formats_convert_to_bgra() {
        assert_eq!(
            convert(wl_shm::Format::Xbgr8888, &[0x11, 0x22, 0x33, 0x44]),
            [0x33, 0x22, 0x11, 0x44]
        );
        assert_eq!(
            convert(wl_shm::Format::Xrgb8888, &[0x33, 0x22, 0x11, 0x44]),
            [0x33, 0x22, 0x11, 0x44]
        );
    }

    /// The layout ledger the calloop polls: a request is unanswered until the control
    /// thread decides an epoch at or past it, the newest verdict stands for every
    /// older request (the layout map is cumulative), and a stale verdict arriving
    /// after a newer one cannot overwrite it.
    #[test]
    fn layout_ledger_answers_by_epoch() {
        let mut ledger = LayoutLedger::default();
        let first = ledger.issue();
        let second = ledger.issue();
        assert_eq!((first, second), (1, 2));
        assert_eq!(ledger.outcome(first), None);
        assert_eq!(ledger.outcome(second), None);

        ledger.decide(first, true);
        assert_eq!(ledger.outcome(first), Some(true));
        assert_eq!(ledger.outcome(second), None);

        // The control thread coalesces queued requests and answers only the newest;
        // that verdict covers the older epoch too.
        let third = ledger.issue();
        ledger.decide(third, false);
        assert_eq!(ledger.outcome(second), Some(false));
        assert_eq!(ledger.outcome(third), Some(false));

        // A late verdict for an older epoch does not roll the ledger back.
        ledger.decide(second, true);
        assert_eq!(ledger.outcome(third), Some(false));
        let fourth = ledger.issue();
        assert_eq!(ledger.outcome(fourth), None);
    }

    /// A point on a display captured at an output's mode, scale 2, beside another: the
    /// stream's logical space (the portal's `size`) holds it at half, clamped to its edge.
    #[test]
    fn stream_point_maps_to_logical_size() {
        let slot = LayoutSlot {
            want: (4080, 2304),
            pos: (1920, 0),
            active: true,
            zero_copy: false,
            paint_cursor: false,
            fps_milli: 60_000,
        };
        assert_eq!(
            stream_point(&slot, (2040, 1152), 1920.0 + 3000.0, 2000.0),
            (1500.0, 1000.0)
        );
        assert_eq!(
            stream_point(&slot, (2040, 1152), 1920.0 + 9000.0, -5.0),
            (2039.0, 0.0)
        );
    }
}
