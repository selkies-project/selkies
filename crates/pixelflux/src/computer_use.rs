/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! HTTP server implementing the [Anthropic Computer Use](https://github.com/anthropics/claude-quickstarts/tree/main/computer-use-demo) specification.
//!
//! Enabled by setting the `PIXELFLUX_CU` environment variable to a bind: comma-separated entries,
//! each a port, served on the loopback addresses only, or `host:port`, the address to serve on
//! (`0.0.0.0:5000,[::]:5000` serves every interface). The server
//! handles `POST /computer-use` requests for screenshots, mouse/keyboard injection, scrolling,
//! and cursor position queries. Actions run against a [`CuBackend`], resolved per request: the
//! Wayland compositor owned by this process when one is registered, otherwise the X server named
//! by `DISPLAY` (XTEST injection on a private connection, no active capture required).
//!
//! A caller drives the session with its owner's full authority, and a loopback address is open to
//! every account on the host, so the server does not start without a token (`PIXELFLUX_CU_TOKEN`,
//! or the caller's own through [`start_cu_server`]) and every request has to carry it as
//! `Authorization: Bearer <token>`.
//!
//! The same server also exposes the built-in MP4 recorder at `/record_start`, `/record_stop`,
//! and `/record_status`, so a headless script can drive a session and record it over plain HTTP.
//! A recording the caller names is a file name inside `PIXELFLUX_RECORD_DIR`, the directory the
//! operator gives callers, and nowhere else.

use std::collections::HashMap;
use std::io::Cursor;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use smithay::input::keyboard::xkb;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use image::{ImageBuffer, ImageFormat, Rgba};
use serde::Deserialize;

use crate::ThreadCommand;
use crate::wayland::keymap::keysym_for_char;

fn clamp<T: PartialOrd>(v: T, lo: T, hi: T) -> T {
    if v < lo {
        lo
    } else if v > hi {
        hi
    } else {
        v
    }
}

/// Turn a raw framebuffer into a PNG the Computer Use agent can actually look at.
///
/// The `screenshot` and `zoom` crop paths have to hand the agent an image, and the API carries it
/// as base64 PNG, so this encodes a flat RGBA buffer (with its dimensions) through the `image`
/// crate and returns the PNG bytes for that response payload.
pub fn encode_png_rgba(data: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    let img = ImageBuffer::<Rgba<u8>, _>::from_raw(width, height, data)
        .ok_or("Failed to create image buffer")?;
    let mut png = Vec::new();
    img.write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
        .map_err(|e| format!("PNG encode error: {}", e))?;
    Ok(png)
}

pub(crate) const MAX_SCREENSHOT_REQUESTS: usize = 4;
const MAX_SCREENSHOT_REQUESTS_PER_DISPLAY: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScreenshotPixelFormat {
    Rgba,
    Bgra,
    Rgbx,
    Bgrx,
}

/// The pixel buffers screenshots hand to their callers, kept when a caller is done with one so
/// the next screenshot neither allocates nor faults in a whole frame (33 MB at 4K) on the
/// compositor. Two are kept, as many as one display admits at once.
pub(crate) struct ScreenshotBuffers {
    kept: Mutex<Vec<Vec<u8>>>,
}

const SCREENSHOT_BUFFERS_KEPT: usize = MAX_SCREENSHOT_REQUESTS_PER_DISPLAY;

static SCREENSHOT_BUFFERS: ScreenshotBuffers = ScreenshotBuffers::new();

impl ScreenshotBuffers {
    const fn new() -> Self {
        Self {
            kept: Mutex::new(Vec::new()),
        }
    }

    fn take(&'static self, len: usize) -> ScreenshotBuffer {
        let mut pixels = self.kept.lock().unwrap().pop().unwrap_or_default();
        // Only bytes past a kept buffer's length are written: every caller fills it whole.
        if pixels.len() > len {
            pixels.truncate(len);
        } else {
            pixels.resize(len, 0);
        }
        ScreenshotBuffer { pixels, home: self }
    }

    fn keep(&self, pixels: Vec<u8>) {
        let mut kept = self.kept.lock().unwrap();
        if kept.len() < SCREENSHOT_BUFFERS_KEPT && pixels.capacity() > 0 {
            kept.push(pixels);
        }
    }
}

/// One screenshot's pixels, returned to the kept buffers when dropped.
pub struct ScreenshotBuffer {
    pixels: Vec<u8>,
    home: &'static ScreenshotBuffers,
}

impl ScreenshotBuffer {
    /// A buffer of `len` bytes for the caller to fill whole.
    pub(crate) fn with_len(len: usize) -> Self {
        SCREENSHOT_BUFFERS.take(len)
    }

    /// A buffer holding a copy of `pixels`.
    pub(crate) fn copy_of(pixels: &[u8]) -> Self {
        let mut buffer = Self::with_len(pixels.len());
        buffer.copy_from_slice(pixels);
        buffer
    }
}

impl From<Vec<u8>> for ScreenshotBuffer {
    fn from(pixels: Vec<u8>) -> Self {
        Self {
            pixels,
            home: &SCREENSHOT_BUFFERS,
        }
    }
}

impl std::ops::Deref for ScreenshotBuffer {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.pixels
    }
}

impl std::ops::DerefMut for ScreenshotBuffer {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.pixels
    }
}

impl Drop for ScreenshotBuffer {
    fn drop(&mut self) {
        self.home.keep(std::mem::take(&mut self.pixels));
    }
}

/// A pixel-pack buffer the compositor's GL readback lands in, mapped persistently so the
/// screenshot's caller copies the pixels out on its own thread. The mapping is valid while
/// `mapped` is set; the compositor clears it, under the lock a copy holds, before it deletes the
/// buffer or its GL context goes away.
pub(crate) struct ReadbackSlot {
    pub(crate) id: u32,
    pub(crate) size: usize,
    ptr: usize,
    mapped: Mutex<bool>,
}

impl ReadbackSlot {
    /// # Safety
    /// `ptr` must address `size` readable bytes until [`ReadbackSlot::unmap`] returns.
    pub(crate) unsafe fn new(id: u32, ptr: *const u8, size: usize) -> Self {
        Self {
            id,
            size,
            ptr: ptr as usize,
            mapped: Mutex::new(true),
        }
    }

    /// Whether a screenshot still holds the slot, which the compositor then neither reuses nor
    /// deletes.
    pub(crate) fn in_use(self: &Arc<Self>) -> bool {
        Arc::strong_count(self) > 1
    }

    /// Forget the mapping, waiting out a copy under way.
    pub(crate) fn unmap(&self) {
        *self.mapped.lock().unwrap() = false;
    }

    pub(crate) fn copy_out(&self, len: usize) -> Result<ScreenshotBuffer, String> {
        let mapped = self.mapped.lock().unwrap();
        if !*mapped {
            return Err("Screenshot compositor stopped".to_string());
        }
        if len > self.size {
            return Err("Screenshot readback produced incomplete pixels".to_string());
        }
        // SAFETY: the mapping stays readable while `mapped` is held set (see `new`).
        let pixels = unsafe { std::slice::from_raw_parts(self.ptr as *const u8, len) };
        Ok(ScreenshotBuffer::copy_of(pixels))
    }
}

/// A screenshot the GPU is still reading back into a [`ReadbackSlot`].
pub struct GpuReadback {
    pub(crate) slot: Arc<ReadbackSlot>,
    pub(crate) sync: smithay::backend::renderer::sync::SyncPoint,
    pub(crate) len: usize,
}

impl GpuReadback {
    /// Wait for the readback until `deadline`, then copy the pixels out of the slot.
    fn resolve(self, deadline: Instant) -> Result<ScreenshotBuffer, String> {
        while !self.sync.is_reached() {
            if Instant::now() >= deadline {
                return Err("Screenshot timed out".to_string());
            }
            thread::sleep(Duration::from_micros(200));
        }
        self.slot.copy_out(self.len)
    }
}

/// A screenshot's pixels: in the caller's buffer, or still on their way from the GPU.
pub enum ScreenshotPixels {
    Ready(ScreenshotBuffer),
    Gpu(GpuReadback),
}

impl From<Vec<u8>> for ScreenshotPixels {
    fn from(pixels: Vec<u8>) -> Self {
        Self::Ready(pixels.into())
    }
}

/// An owned copy of one rendered output, independent of the streaming buffer pool.
/// Channel conversion and PNG compression run in the requesting thread.
pub struct ScreenshotFrame {
    pub pixels: ScreenshotPixels,
    pub width: u32,
    pub height: u32,
    pub format: ScreenshotPixelFormat,
}

impl ScreenshotFrame {
    /// The frame with its pixels in the caller's buffer, a GPU readback waited for until
    /// `deadline` and copied out.
    fn resolve(self, deadline: Instant) -> Result<Self, String> {
        let pixels = match self.pixels {
            ScreenshotPixels::Ready(pixels) => pixels,
            ScreenshotPixels::Gpu(readback) => readback.resolve(deadline)?,
        };
        Ok(Self {
            pixels: ScreenshotPixels::Ready(pixels),
            ..self
        })
    }

    fn encode_png(self) -> Result<Vec<u8>, String> {
        let ScreenshotPixels::Ready(mut pixels) = self.pixels else {
            return Err("Screenshot pixels are still being read back".to_string());
        };
        let expected = (self.width as usize)
            .checked_mul(self.height as usize)
            .and_then(|n| n.checked_mul(4));
        if self.width == 0 || self.height == 0 || expected != Some(pixels.len()) {
            return Err("Screenshot render produced an invalid framebuffer".to_string());
        }
        let swap = matches!(
            self.format,
            ScreenshotPixelFormat::Bgra | ScreenshotPixelFormat::Bgrx
        );
        let opaque = matches!(
            self.format,
            ScreenshotPixelFormat::Rgbx | ScreenshotPixelFormat::Bgrx
        );
        if swap || opaque {
            for px in pixels.as_chunks_mut::<4>().0 {
                if swap {
                    px.swap(0, 2);
                }
                if opaque {
                    px[3] = 255;
                }
            }
        }
        encode_png_rgba(&pixels, self.width, self.height)
    }
}

/// Bound raw snapshots and PNG work together, including callers still compressing a reply.
/// Two requests per output allow a readback while its previous response is compressed;
/// four across all outputs bound concurrent frame copies without allocating worker threads.
#[derive(Default)]
struct ScreenshotAdmission {
    counts: Mutex<HashMap<u32, usize>>,
}

impl ScreenshotAdmission {
    fn acquire(&self, display: u32) -> Result<ScreenshotPermit<'_>, String> {
        let mut counts = self.counts.lock().unwrap();
        if counts.values().sum::<usize>() >= MAX_SCREENSHOT_REQUESTS
            || counts.get(&display).copied().unwrap_or(0) >= MAX_SCREENSHOT_REQUESTS_PER_DISPLAY
        {
            return Err("Screenshot busy".to_string());
        }
        *counts.entry(display).or_default() += 1;
        Ok(ScreenshotPermit {
            admission: self,
            display,
            canceled: Arc::new(AtomicBool::new(false)),
        })
    }
}

struct ScreenshotPermit<'a> {
    admission: &'a ScreenshotAdmission,
    display: u32,
    canceled: Arc<AtomicBool>,
}

impl Drop for ScreenshotPermit<'_> {
    fn drop(&mut self) {
        self.canceled.store(true, Ordering::Release);
        let mut counts = self.admission.counts.lock().unwrap();
        if let Some(count) = counts.get_mut(&self.display) {
            *count -= 1;
            if *count == 0 {
                counts.remove(&self.display);
            }
        }
    }
}

static SCREENSHOT_ADMISSION: OnceLock<ScreenshotAdmission> = OnceLock::new();

fn scancode_for_keyname(name: &str) -> Option<u32> {
    Some(match name.to_lowercase().as_str() {
        "return" | "enter" => 36,
        "tab" => 23,
        "escape" | "esc" => 9,
        "backspace" => 22,
        "delete" => 119,
        "insert" => 118,
        "home" => 110,
        "end" => 115,
        "pageup" => 112,
        "pagedown" => 117,
        "up" => 111,
        "down" => 116,
        "left" => 113,
        "right" => 114,
        "space" => 65,
        "capslock" => 66,
        "numlock" => 77,
        "scrolllock" => 78,
        "printscreen" | "sysrq" | "print" => 107,
        "pause" | "break" => 127,
        "f1" => 67,
        "f2" => 68,
        "f3" => 69,
        "f4" => 70,
        "f5" => 71,
        "f6" => 72,
        "f7" => 73,
        "f8" => 74,
        "f9" => 75,
        "f10" => 76,
        "f11" => 95,
        "f12" => 96,
        "f13" => 191,
        "f14" => 192,
        "f15" => 193,
        "f16" => 194,
        "f17" => 195,
        "f18" => 196,
        "f19" => 197,
        "f20" => 198,
        "f21" => 199,
        "f22" => 200,
        "f23" => 201,
        "f24" => 202,
        "ctrl" | "lctrl" | "leftctrl" => 37,
        "rctrl" | "rightctrl" => 105,
        "shift" | "lshift" | "leftshift" => 50,
        "rshift" | "rightshift" => 62,
        "alt" | "lalt" | "leftalt" => 64,
        "ralt" | "rightalt" => 108,
        "super" | "meta" | "lsuper" | "leftmeta" | "leftsuper" | "windows" | "leftwindows" => 133,
        "rsuper" | "rightmeta" | "rightsuper" | "rightwindows" => 134,
        "menu" | "compose" => 135,
        "kp_0" | "kp0" => 90,
        "kp_1" | "kp1" => 87,
        "kp_2" | "kp2" => 88,
        "kp_3" | "kp3" => 89,
        "kp_4" | "kp4" => 83,
        "kp_5" | "kp5" => 84,
        "kp_6" | "kp6" => 85,
        "kp_7" | "kp7" => 79,
        "kp_8" | "kp8" => 80,
        "kp_9" | "kp9" => 81,
        "kp_decimal" | "kp_dot" => 91,
        "kp_divide" | "kp_slash" => 106,
        "kp_multiply" | "kp_asterisk" => 63,
        "kp_subtract" | "kp_minus" => 82,
        "kp_add" | "kp_plus" => 86,
        "kp_enter" => 104,
        _ => return None,
    })
}

/// Keysym for one component of an xdotool-style key spec that is not a named key: a single
/// literal character (`a`, `7`, `-`, `#`) via the Unicode mapping, or a keysym NAME
/// (`minus`, `bracketleft`, `udiaeresis`, …) via xkb's name lookup (case-tolerant fallback
/// so `Minus` still resolves). Returns 0 when the spec names no keysym.
fn keysym_for_key_spec(name: &str) -> u32 {
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return keysym_for_char(c);
    }
    let sym = xkb::keysym_from_name(name, xkb::KEYSYM_NO_FLAGS).raw();
    if sym != 0 {
        return sym;
    }
    xkb::keysym_from_name(name, xkb::KEYSYM_CASE_INSENSITIVE).raw()
}

fn is_modifier(scancode: u32) -> bool {
    matches!(scancode, 37 | 105 | 50 | 62 | 64 | 108 | 133 | 134)
}

/// Semantic mouse-button vocabulary of the action layer; each backend maps it to its native
/// codes (evdev `BTN_` on Wayland, X core buttons 1/2/3 on X11).
#[derive(Clone, Copy)]
pub enum CuButton {
    Left,
    Right,
    Middle,
}

/// The desktop-side primitives one Computer Use action decomposes into. Keycodes are X/xkb
/// keycodes (evdev + 8) — the numbering both the smithay seat and XTEST consume — and
/// coordinates are framebuffer/root pixels, already clamped by the action layer.
pub trait CuBackend {
    fn name(&self) -> &'static str;
    fn fb_size(&self) -> Result<(i32, i32), String>;
    /// One display's framebuffer size; 0 = the primary. Unknown ids are an error. The
    /// X11 backend serves only the root (display 0); Wayland resolves any live output id.
    fn display_fb_size(&self, display: u32) -> Result<(i32, i32), String> {
        if display == 0 {
            self.fb_size()
        } else {
            Err(format!("Unknown display: {display}"))
        }
    }
    fn key(&self, scancode: u32, pressed: bool);
    fn mouse_move(&self, x: f64, y: f64);
    fn button(&self, btn: CuButton, pressed: bool);
    fn scroll(&self, dx: f64, dy: f64);
    /// PNG of one display's framebuffer; 0 = the primary. Unknown ids are an error.
    fn screenshot_png(&self, display: u32) -> Result<Vec<u8>, String>;
    fn cursor_pos(&self) -> Result<(f64, f64), String>;
    /// Run `seq` with every keysym in `keysyms` (which `resolve_keysyms` could not place)
    /// made temporarily typeable, when the backend can arrange that; `seq` receives
    /// keysym -> keycode for the transient bindings (empty when none could be arranged,
    /// in which case those keysyms simply stay untypeable). Wayland's `resolve_keysyms`
    /// already overlay-binds anything the base keymap lacks, so the default runs `seq`
    /// with no bindings; the X11 backend overrides this with a grab-atomic spare-keycode
    /// remap.
    fn with_transient_keysyms(&self, keysyms: &[u32], seq: &mut dyn FnMut(&HashMap<u32, u32>)) {
        let _ = keysyms;
        seq(&HashMap::new());
    }
    /// Resolve keysyms against the backend's ACTIVE keymap: one `(keycode, shift level)`
    /// per input keysym, `(0, 0)` where the keymap cannot produce it. Level bit 0 = Shift,
    /// bit 1 = AltGr — the order xkb two/four-level key types use. Wayland resolves through
    /// the compositor's keymap policy (overlay-binding what the base lacks); X11 through
    /// the server's own `GetKeyboardMapping`.
    fn resolve_keysyms(&self, keysyms: &[u32]) -> Vec<(u32, u32)>;
    /// Keycode synthesized around AltGr-level hits (level bit 1). The default is the
    /// pc105 right-Alt position the compositor's seat keymap binds to `ISO_Level3_Shift`;
    /// the X11 backend overrides it with the level-3 modifier key found in the server's
    /// keymap (0 = none, in which case `resolve_keysyms` reports no AltGr levels).
    fn altgr_keycode(&self) -> u32 {
        KC_ALTGR
    }
}

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

/// Reply deadline for compositor round-trips (screenshot readback, cursor position, fb size).
/// Bounded so a wedged render path turns into a JSON error instead of hanging the sequential
/// HTTP loop — and every request behind it — forever.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// Wayland implementation: every primitive is a `ThreadCommand` on the compositor's calloop
/// channel, so injection and readback serialize naturally with rendering and encoding.
#[derive(Clone)]
pub struct CuWaylandBackend {
    tx: smithay::reexports::calloop::channel::Sender<ThreadCommand>,
    wake_tx: smithay::reexports::calloop::channel::Sender<()>,
}

impl CuBackend for CuWaylandBackend {
    fn name(&self) -> &'static str {
        "wayland"
    }

    fn fb_size(&self) -> Result<(i32, i32), String> {
        self.display_fb_size(0)
    }

    fn display_fb_size(&self, display: u32) -> Result<(i32, i32), String> {
        let (resp_tx, resp_rx) = mpsc::channel();
        self.tx
            .send(ThreadCommand::CuGetInfo {
                display_id: display,
                resp: resp_tx,
            })
            .map_err(|_| "Failed to request compositor info".to_string())?;
        let (w, h, _) = resp_rx
            .recv_timeout(REPLY_TIMEOUT)
            .map_err(|_| "Compositor info request failed".to_string())?;
        // CuGetInfo reports zeros for output ids that don't exist (and for an output
        // with no mode, which live outputs always have).
        if w <= 0 || h <= 0 {
            return Err(format!("Unknown display: {display}"));
        }
        Ok((w, h))
    }

    fn key(&self, scancode: u32, pressed: bool) {
        let _ = self.tx.send(ThreadCommand::KeyboardKey {
            scancode,
            state: if pressed { 1 } else { 0 },
        });
    }

    fn mouse_move(&self, x: f64, y: f64) {
        let _ = self
            .tx
            .send(ThreadCommand::PointerMotion { x, y, motion: 0 });
    }

    fn button(&self, btn: CuButton, pressed: bool) {
        let btn = match btn {
            CuButton::Left => BTN_LEFT,
            CuButton::Right => BTN_RIGHT,
            CuButton::Middle => BTN_MIDDLE,
        };
        let _ = self.tx.send(ThreadCommand::PointerButton {
            btn,
            state: if pressed { 1 } else { 0 },
        });
    }

    fn scroll(&self, dx: f64, dy: f64) {
        let _ = self.tx.send(ThreadCommand::PointerAxis { x: dx, y: dy });
    }

    fn screenshot_png(&self, display: u32) -> Result<Vec<u8>, String> {
        let permit = SCREENSHOT_ADMISSION
            .get_or_init(ScreenshotAdmission::default)
            .acquire(display)?;
        let deadline = Instant::now() + REPLY_TIMEOUT;
        let (resp_tx, resp_rx) = mpsc::channel();
        self.tx
            .send(ThreadCommand::CuScreenshot {
                display_id: display,
                resp: resp_tx,
                canceled: Arc::clone(&permit.canceled),
                deadline,
            })
            .map_err(|_| "Failed to request screenshot".to_string())?;
        let _ = self.wake_tx.send(());
        let frame = resp_rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => "Screenshot timed out".to_string(),
                mpsc::RecvTimeoutError::Disconnected => {
                    "Screenshot compositor disconnected".to_string()
                }
            })??;
        let frame = frame.resolve(deadline)?;
        if Instant::now() >= deadline {
            return Err("Screenshot timed out".to_string());
        }
        frame.encode_png()
    }

    fn cursor_pos(&self) -> Result<(f64, f64), String> {
        let (resp_tx, resp_rx) = mpsc::channel();
        self.tx
            .send(ThreadCommand::CuCursorPosition { resp: resp_tx })
            .map_err(|_| "Failed to request cursor position".to_string())?;
        resp_rx
            .recv_timeout(REPLY_TIMEOUT)
            .map_err(|_| "Cursor position failed".to_string())
    }

    fn resolve_keysyms(&self, keysyms: &[u32]) -> Vec<(u32, u32)> {
        let (resp_tx, resp_rx) = mpsc::channel();
        if self
            .tx
            .send(ThreadCommand::BindKeysyms {
                keysyms: keysyms.to_vec(),
                reply: resp_tx,
            })
            .is_err()
        {
            return vec![(0, 0); keysyms.len()];
        }
        resp_rx
            .recv_timeout(REPLY_TIMEOUT)
            .unwrap_or_else(|_| vec![(0, 0); keysyms.len()])
    }
}

/// Distinct characters a `type` request binds and types at a time: each run's binds then fit
/// the seat's text spares (`keymap::TEXT_SPARE_KEYCODES`; 9 free on a Japanese layout, 11 on
/// most), on which every client types, where a bind past them lands above 255 and Chromium
/// drops it.
const TYPE_RUN_KEYSYMS: usize = 8;

/// `syms` cut into consecutive runs of at most `max` distinct keysyms.
fn distinct_runs(syms: &[u32], max: usize) -> Vec<&[u32]> {
    let mut runs = Vec::new();
    let (mut start, mut seen) = (0, std::collections::HashSet::new());
    for (i, sym) in syms.iter().enumerate() {
        if !seen.contains(sym) && seen.len() == max {
            runs.push(&syms[start..i]);
            start = i;
            seen.clear();
        }
        seen.insert(*sym);
    }
    if start < syms.len() {
        runs.push(&syms[start..]);
    }
    runs
}

/// Per-request literal-key resolver: batches keysym lookups against the backend's active
/// keymap and caches them for the request's burst of key events (a fresh backend — and thus
/// a fresh cache — is resolved per HTTP request, so a runtime layout switch is picked up by
/// the next request).
struct KeyResolver<'a> {
    backend: &'a dyn CuBackend,
    cache: HashMap<u32, (u32, u32)>,
}

impl<'a> KeyResolver<'a> {
    fn new(backend: &'a dyn CuBackend) -> Self {
        Self {
            backend,
            cache: HashMap::new(),
        }
    }

    /// Resolve a batch up front so a `type` action costs one backend round trip.
    fn prefetch(&mut self, keysyms: &[u32]) {
        let missing: Vec<u32> = {
            let mut seen = std::collections::HashSet::new();
            keysyms
                .iter()
                .copied()
                .filter(|&s| s != 0 && !self.cache.contains_key(&s) && seen.insert(s))
                .collect()
        };
        if missing.is_empty() {
            return;
        }
        let out = self.backend.resolve_keysyms(&missing);
        for (sym, hit) in missing.into_iter().zip(out) {
            self.cache.insert(sym, hit);
        }
    }

    /// `(keycode, shift level)` for `keysym`, or `None` when the active keymap cannot
    /// produce it.
    fn resolve(&mut self, keysym: u32) -> Option<(u32, u32)> {
        if keysym == 0 {
            return None;
        }
        self.prefetch(&[keysym]);
        let hit = self.cache.get(&keysym).copied().unwrap_or((0, 0));
        (hit.0 != 0).then_some(hit)
    }
}

/// Shift and default AltGr (pc105 right Alt / ISO_Level3_Shift) keycodes, held to reach a
/// resolved key's shift level: bit 0 = Shift, bit 1 = AltGr.
const KC_SHIFT: u32 = 50;
const KC_ALTGR: u32 = 108;

/// The level modifiers `level` requires beyond whatever is already held (`held_mask` uses
/// the same bit layout), in press order. `altgr` is the backend's AltGr keycode
/// ([`CuBackend::altgr_keycode`]).
fn level_modifiers(level: u32, held_mask: u32, altgr: u32) -> Vec<u32> {
    let mut out = Vec::new();
    if level & 1 != 0 && held_mask & 1 == 0 {
        out.push(KC_SHIFT);
    }
    if level & 2 != 0 && held_mask & 2 == 0 && altgr != 0 {
        out.push(altgr);
    }
    out
}

#[derive(Deserialize)]
struct CuActionRequest {
    action: String,
    coordinate: Option<[f64; 2]>,
    start_coordinate: Option<[f64; 2]>,
    text: Option<String>,
    key: Option<String>,
    scroll_direction: Option<String>,
    scroll_amount: Option<i32>,
    duration: Option<f64>,
    region: Option<[f64; 4]>,
    /// Display id for `screenshot` / `zoom` (default 0, the primary; `record_start` uses
    /// its own `display` field with the same meaning). Unknown ids get an error reply.
    display: Option<u32>,
}

fn ok_json() -> String {
    "{\"result\":\"ok\"}".to_string()
}

fn handle_action(req: CuActionRequest, backend: &dyn CuBackend) -> String {
    let result = handle_action_inner(req, backend);
    match result {
        Ok(response) => response,
        Err(e) => format!("{{\"error\":\"{}\"}}", e.replace('"', "\\\"")),
    }
}

/// Turn one parsed Computer Use request into a real action on the captured desktop — the
/// bridge that lets an external AI agent see and drive the session.
///
/// Every action ultimately becomes a backend input primitive or a framebuffer read, so this is
/// where the API's vocabulary meets the running desktop. Coordinates are clamped to the current
/// framebuffer so a mistaken agent click cannot address off-screen pixels; keyboard and pointer
/// actions translate into input events; and actions that must look at the screen (`screenshot`,
/// `zoom`) request a fresh capture first, so the agent reasons about the current frame rather than
/// a stale one. Returns the JSON response, or an error string explaining why the action could not
/// run.
fn handle_action_inner(req: CuActionRequest, b: &dyn CuBackend) -> Result<String, String> {
    let sleep_ms = |ms: u64| thread::sleep(Duration::from_millis(ms));

    let (fb_w, fb_h) = b.fb_size()?;

    let handle_coord = |coord: [f64; 2]| -> (f64, f64) {
        (
            clamp(coord[0], 0.0, (fb_w - 1) as f64),
            clamp(coord[1], 0.0, (fb_h - 1) as f64),
        )
    };

    let handle_modifier = |mod_name: &str| -> Option<u32> {
        let sc = scancode_for_keyname(mod_name)?;
        if is_modifier(sc) { Some(sc) } else { None }
    };

    match req.action.as_str() {
        "screenshot" => {
            let png = b.screenshot_png(req.display.unwrap_or(0))?;
            let b64 = BASE64.encode(&png);
            Ok(format!("{{\"data\":\"{}\"}}", b64))
        }

        "mouse_move" => {
            let coord = req.coordinate.ok_or("Missing coordinate")?;
            let (fx, fy) = handle_coord(coord);
            b.mouse_move(fx, fy);
            Ok(ok_json())
        }

        "left_click" | "right_click" | "middle_click" => {
            let btn = match req.action.as_str() {
                "left_click" => CuButton::Left,
                "right_click" => CuButton::Right,
                _ => CuButton::Middle,
            };
            if let Some(coord) = req.coordinate {
                let (fx, fy) = handle_coord(coord);
                b.mouse_move(fx, fy);
                sleep_ms(30);
            }
            if let Some(ref mod_name) = req.text
                && let Some(sc) = handle_modifier(mod_name)
            {
                b.key(sc, true);
                sleep_ms(20);
            }
            b.button(btn, true);
            sleep_ms(20);
            b.button(btn, false);
            if let Some(ref mod_name) = req.text
                && let Some(sc) = handle_modifier(mod_name)
            {
                sleep_ms(10);
                b.key(sc, false);
            }
            Ok(ok_json())
        }

        "double_click" | "triple_click" => {
            let n = if req.action == "double_click" { 2 } else { 3 };
            if let Some(coord) = req.coordinate {
                let (fx, fy) = handle_coord(coord);
                b.mouse_move(fx, fy);
                sleep_ms(30);
            }
            if let Some(ref mod_name) = req.text
                && let Some(sc) = handle_modifier(mod_name)
            {
                b.key(sc, true);
                sleep_ms(20);
            }
            for _ in 0..n {
                b.button(CuButton::Left, true);
                sleep_ms(10);
                b.button(CuButton::Left, false);
                sleep_ms(10);
            }
            if let Some(ref mod_name) = req.text
                && let Some(sc) = handle_modifier(mod_name)
            {
                sleep_ms(10);
                b.key(sc, false);
            }
            Ok(ok_json())
        }

        "left_click_drag" => {
            let start = req.start_coordinate.ok_or("Missing start_coordinate")?;
            let end = req.coordinate.ok_or("Missing coordinate")?;
            let (sx, sy) = handle_coord(start);
            let (ex, ey) = handle_coord(end);
            b.mouse_move(sx, sy);
            sleep_ms(30);
            b.button(CuButton::Left, true);
            sleep_ms(30);
            b.mouse_move(ex, ey);
            sleep_ms(30);
            b.button(CuButton::Left, false);
            Ok(ok_json())
        }

        "left_mouse_down" => {
            b.button(CuButton::Left, true);
            Ok(ok_json())
        }

        "left_mouse_up" => {
            b.button(CuButton::Left, false);
            Ok(ok_json())
        }

        "type" => {
            let text = req.text.as_deref().ok_or("Missing text")?;
            // A nested app compositor owns the apps on its own socket; type there as
            // a virtual-keyboard client, since keys on pixelflux's own seat carry an
            // overlay keymap the inner compositor never sees. Falls through to the
            // local seat if the app socket is unreachable.
            if b.name() == "wayland"
                && let Some(sock) = app_wayland_socket_path()
            {
                // Failures log once per socket value; every request still
                // retries, so a compositor that comes back is used again
                // immediately (and re-arms the logging).
                static FAILED_SOCK: Mutex<Option<String>> = Mutex::new(None);
                match crate::wayland::vkclient::type_text_to(&sock, text) {
                    Ok(()) => {
                        *FAILED_SOCK.lock().unwrap() = None;
                        return Ok(ok_json());
                    }
                    Err(e) => {
                        let mut last = FAILED_SOCK.lock().unwrap();
                        if last.as_deref() != Some(sock.as_str()) {
                            eprintln!(
                                "[ComputerUse] app-compositor type via {sock} failed ({e}); using local seat until it is reachable"
                            );
                            *last = Some(sock);
                        }
                    }
                }
            }
            let all: Vec<u32> = text.chars().map(keysym_for_char).collect();
            for syms in distinct_runs(&all, TYPE_RUN_KEYSYMS) {
                let mut resolver = KeyResolver::new(b);
                resolver.prefetch(syms);
                // Base+AltGr resolution stays the preferred path; only what the active keymap
                // cannot reach at all goes through the backend's transient-bind fallback.
                let mut unresolved: Vec<u32> = Vec::new();
                for &sym in syms {
                    if sym != 0 && resolver.resolve(sym).is_none() && !unresolved.contains(&sym) {
                        unresolved.push(sym);
                    }
                }
                b.with_transient_keysyms(&unresolved, &mut |bound| {
                    for (i, &sym) in syms.iter().enumerate() {
                        if i > 0 && i % 50 == 0 {
                            sleep_ms(20);
                        }
                        if let Some((sc, level)) = resolver.resolve(sym) {
                            let level_mods = level_modifiers(level, 0, b.altgr_keycode());
                            for &m in &level_mods {
                                b.key(m, true);
                                sleep_ms(5);
                            }
                            b.key(sc, true);
                            sleep_ms(10);
                            b.key(sc, false);
                            for &m in level_mods.iter().rev() {
                                b.key(m, false);
                            }
                            sleep_ms(8);
                        } else if let Some(&kc) = bound.get(&sym) {
                            // Transient binds sit at the plain level: no modifiers needed.
                            b.key(kc, true);
                            sleep_ms(10);
                            b.key(kc, false);
                            sleep_ms(8);
                        }
                    }
                });
            }
            Ok(ok_json())
        }

        "key" => {
            let text = req.text.as_deref().ok_or("Missing text")?;
            let mut resolver = KeyResolver::new(b);
            let mut mods: Vec<u32> = Vec::new();
            let mut main_key: Option<(u32, u32)> = None;
            // Keysym of the last main-key spec the active keymap could not place (cleared
            // when a later part resolves): typed via the transient-bind fallback.
            let mut unresolved_sym: Option<u32> = None;
            for part in text.split('+') {
                let trimmed = part.trim();
                if let Some(sc) = scancode_for_keyname(trimmed) {
                    if is_modifier(sc) {
                        mods.push(sc);
                    } else {
                        main_key = Some((sc, 0));
                        unresolved_sym = None;
                    }
                } else {
                    let sym = keysym_for_key_spec(trimmed);
                    if let Some(hit) = resolver.resolve(sym) {
                        main_key = Some(hit);
                        unresolved_sym = None;
                    } else if sym != 0 {
                        main_key = None;
                        unresolved_sym = Some(sym);
                    }
                }
            }
            let unresolved: Vec<u32> = unresolved_sym.into_iter().collect();
            b.with_transient_keysyms(&unresolved, &mut |bound| {
                let main_key = main_key.or_else(|| {
                    // Transient binds sit at the plain level: no modifiers needed.
                    unresolved_sym
                        .and_then(|s| bound.get(&s))
                        .map(|&kc| (kc, 0))
                });
                for &sc in &mods {
                    b.key(sc, true);
                    sleep_ms(10);
                }
                if let Some((sc, level)) = main_key {
                    // Modifiers reaching the key's shift level are added unless the spec
                    // already asked for them (Shift as 50/62, AltGr as 108 or the backend's
                    // resolved level-3 keycode).
                    let altgr = b.altgr_keycode();
                    let held = mods.iter().fold(0u32, |m, &sc| match sc {
                        50 | 62 => m | 1,
                        sc if sc == KC_ALTGR || sc == altgr => m | 2,
                        _ => m,
                    });
                    let level_mods = level_modifiers(level, held, altgr);
                    for &m in &level_mods {
                        b.key(m, true);
                        sleep_ms(10);
                    }
                    b.key(sc, true);
                    sleep_ms(30);
                    b.key(sc, false);
                    for &m in level_mods.iter().rev() {
                        sleep_ms(10);
                        b.key(m, false);
                    }
                } else if let Some(&last_mod) = mods.last() {
                    b.key(last_mod, true);
                    sleep_ms(30);
                    b.key(last_mod, false);
                }
                for &sc in mods.iter().rev() {
                    sleep_ms(10);
                    b.key(sc, false);
                }
            });
            Ok(ok_json())
        }

        "hold_key" => {
            let text = req.text.as_deref().ok_or("Missing text")?;
            let duration = req.duration.ok_or("Missing duration")?;
            let duration = duration.min(100.0);
            let trimmed = text.trim();
            let mut resolver = KeyResolver::new(b);
            let hit = match scancode_for_keyname(trimmed) {
                Some(sc) => Some((sc, 0)),
                None => resolver.resolve(keysym_for_key_spec(trimmed)),
            };
            if let Some((sc, level)) = hit {
                let level_mods = level_modifiers(level, 0, b.altgr_keycode());
                for &m in &level_mods {
                    b.key(m, true);
                    sleep_ms(10);
                }
                b.key(sc, true);
                sleep_ms((duration * 1000.0) as u64);
                b.key(sc, false);
                for &m in level_mods.iter().rev() {
                    sleep_ms(10);
                    b.key(m, false);
                }
            }
            Ok(ok_json())
        }

        "scroll" => {
            let dir = req
                .scroll_direction
                .as_deref()
                .ok_or("Missing scroll_direction")?;
            let amount = req.scroll_amount.unwrap_or(1).max(0) as f64;
            if let Some(coord) = req.coordinate {
                let (fx, fy) = handle_coord(coord);
                b.mouse_move(fx, fy);
                sleep_ms(30);
            }
            if let Some(ref mod_name) = req.text
                && let Some(sc) = handle_modifier(mod_name)
            {
                b.key(sc, true);
                sleep_ms(20);
            }
            let (dx, dy) = match dir {
                "up" => (0.0, -amount),
                "down" => (0.0, amount),
                "left" => (-amount, 0.0),
                "right" => (amount, 0.0),
                _ => return Err(format!("Invalid scroll_direction: {}", dir)),
            };
            b.scroll(dx, dy);
            sleep_ms(30);
            if let Some(ref mod_name) = req.text
                && let Some(sc) = handle_modifier(mod_name)
            {
                sleep_ms(10);
                b.key(sc, false);
            }
            Ok(ok_json())
        }

        "cursor_position" => {
            let (x, y) = b.cursor_pos()?;
            Ok(format!(
                "{{\"text\":\"X={},Y={}\"}}",
                x.round() as i32,
                y.round() as i32
            ))
        }

        "wait" => {
            let duration = req.duration.unwrap_or(1.0).min(100.0);
            sleep_ms((duration * 1000.0) as u64);
            Ok(ok_json())
        }

        "zoom" => {
            let region = req.region.ok_or("Missing region")?;
            let display = req.display.unwrap_or(0);
            // The region clamps against the TARGET display's own framebuffer, which need
            // not match the primary's.
            let (dw, dh) = b.display_fb_size(display)?;
            let (x0, y0, x1, y1) = (region[0], region[1], region[2], region[3]);
            let left = clamp(x0.round() as u32, 0, dw as u32 - 1);
            let top = clamp(y0.round() as u32, 0, dh as u32 - 1);
            let right = clamp(x1.round() as u32, left + 1, dw as u32);
            let bottom = clamp(y1.round() as u32, top + 1, dh as u32);
            let crop_w = right - left;
            let crop_h = bottom - top;
            let png = b.screenshot_png(display)?;
            if crop_w > 0
                && crop_h > 0
                && let Ok(img) = image::load_from_memory(&png)
            {
                let cropped = img.crop_imm(left, top, crop_w, crop_h);
                let mut out = Vec::new();
                cropped
                    .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
                    .map_err(|e| format!("Crop/encode error: {}", e))?;
                let b64 = BASE64.encode(&out);
                return Ok(format!("{{\"data\":\"{}\"}}", b64));
            }
            let b64 = BASE64.encode(&png);
            Ok(format!("{{\"data\":\"{}\"}}", b64))
        }

        _ => Err(format!("Unknown action: {}", req.action)),
    }
}

static WAYLAND_BACKEND: Mutex<Option<CuWaylandBackend>> = Mutex::new(None);

/// Body of `POST /record_start`. All fields are optional; unset ones fall back to the
/// `PIXELFLUX_RECORD_*` environment variables and built-in defaults.
#[derive(Deserialize, Default)]
struct RecordStartRequest {
    /// Output file name inside `PIXELFLUX_RECORD_DIR` (default: `$PIXELFLUX_RECORD`, else
    /// `pixelflux-record-<unix_ts>.mp4` in that directory or `/tmp`).
    path: Option<String>,
    /// Wayland output id to record (default 0; ignored on X11).
    display: Option<u32>,
    /// Capture fps cap for a recorder-owned capture.
    fps: Option<f64>,
    /// Bitrate override in kbps for a recorder-owned capture.
    bitrate_kbps: Option<i32>,
    /// Unix socket serving an Ogg Opus stream to record as the audio track.
    audio_socket: Option<String>,
}

/// The path of a recording the caller names `name`: a plain file name inside `dir`, so a request
/// can neither leave the operator's directory nor name a file anywhere when none was given.
fn named_record_path(dir: Option<&str>, name: &str) -> Result<String, String> {
    let dir = dir.ok_or("a recording named by the caller needs PIXELFLUX_RECORD_DIR")?;
    let mut parts = std::path::Path::new(name).components();
    match (parts.next(), parts.next()) {
        (Some(std::path::Component::Normal(file)), None) => Ok(std::path::Path::new(dir)
            .join(file)
            .to_string_lossy()
            .into_owned()),
        _ => Err(format!("'{name}' is not a file name")),
    }
}

/// Handle the recorder REST endpoints sharing the CU server: `record_start`,
/// `record_stop`, and `record_status`, all one JSON round-trip into the same recorder
/// implementation the Python API and env vars use. Returns `None` for any other URL.
fn handle_record_endpoint(url: &str, body: &str) -> Option<String> {
    let reply = match url {
        "/record_start" => {
            let req: RecordStartRequest = if body.trim().is_empty() {
                RecordStartRequest::default()
            } else {
                match serde_json::from_str(body) {
                    Ok(r) => r,
                    Err(e) => return Some(format!("{{\"error\":\"Invalid JSON: {}\"}}", e)),
                }
            };
            let record_dir = std::env::var("PIXELFLUX_RECORD_DIR")
                .ok()
                .filter(|d| !d.is_empty());
            let path = match req.path {
                Some(name) => match named_record_path(record_dir.as_deref(), &name) {
                    Ok(path) => path,
                    Err(e) => return Some(serde_json::json!({ "error": e }).to_string()),
                },
                None => std::env::var("PIXELFLUX_RECORD")
                    .ok()
                    .filter(|p| !p.is_empty())
                    .unwrap_or_else(|| {
                        let ts = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        // The session's runtime directory is its own; /tmp is shared.
                        let runtime = std::env::var("XDG_RUNTIME_DIR")
                            .ok()
                            .filter(|d| !d.is_empty());
                        let dir = record_dir
                            .as_deref()
                            .or(runtime.as_deref())
                            .unwrap_or("/tmp");
                        format!("{dir}/pixelflux-record-{ts}.mp4")
                    }),
            };
            let mut opts = crate::recorder::RecordOptions::from_env(path);
            if let Some(d) = req.display {
                opts.display_id = d;
            }
            if let Some(f) = req.fps {
                opts.fps = f;
            }
            if let Some(b) = req.bitrate_kbps {
                opts.bitrate_kbps = b;
            }
            if let Some(a) = req.audio_socket {
                opts.audio_socket = a;
            }
            match crate::recorder::start(opts) {
                Ok(s) => crate::recorder::status_to_json(&s).to_string(),
                Err(e) => serde_json::json!({ "error": e }).to_string(),
            }
        }
        "/record_stop" => match crate::recorder::stop() {
            Ok(s) => {
                let mut v = crate::recorder::status_to_json(&s);
                v["stopped"] = serde_json::Value::Bool(true);
                v.to_string()
            }
            Err(e) => serde_json::json!({ "error": e }).to_string(),
        },
        "/record_status" => match crate::recorder::status() {
            Some(s) => crate::recorder::status_to_json(&s).to_string(),
            None => serde_json::json!({ "active": false }).to_string(),
        },
        _ => return None,
    };
    Some(reply)
}

/// Socket of the compositor apps run under when it is nested under pixelflux
/// (labwc/kwin): keys injected into pixelflux's own seat carry an overlay keymap
/// the inner compositor never sees, so CU text is typed as a client of this
/// socket instead. Set by selkies over the ScreenCapture ABI.
static CU_APP_WAYLAND_DISPLAY: Mutex<Option<String>> = Mutex::new(None);

/// Set (or clear, with None/empty) the app compositor socket for CU text injection
/// and for relative pointer motion into a nested KWin session.
pub fn set_app_wayland_display(display: Option<String>) {
    *CU_APP_WAYLAND_DISPLAY.lock().unwrap() = display.filter(|s| !s.is_empty());
    crate::wayland::ficlient::arm(app_wayland_socket_path());
}

/// Resolve the app compositor socket PATH for CU typing, or None to type on the
/// local seat. The ABI value selkies set wins; a standalone CU (no selkies) falls
/// back to PIXELFLUX_APP_WAYLAND_DISPLAY. A value naming pixelflux's own
/// compositor means nothing is nested.
pub(crate) fn app_wayland_socket_path() -> Option<String> {
    let name = CU_APP_WAYLAND_DISPLAY
        .lock()
        .unwrap()
        .clone()
        .or_else(|| std::env::var("PIXELFLUX_APP_WAYLAND_DISPLAY").ok())
        .filter(|s| !s.is_empty())?;
    if crate::wait_socket_name(Duration::from_millis(0)).as_deref() == Some(name.as_str()) {
        return None;
    }
    crate::wayland::wlclient::socket_path(&name)
}

/// Make the Wayland compositor the preferred CU backend: once a live calloop sender is
/// registered, every subsequent request routes to it instead of an X11 connection.
pub fn register_wayland_backend(
    tx: smithay::reexports::calloop::channel::Sender<ThreadCommand>,
    wake_tx: smithay::reexports::calloop::channel::Sender<()>,
) {
    *WAYLAND_BACKEND.lock().unwrap() = Some(CuWaylandBackend { tx, wake_tx });
}

/// The registered compositor's command channel, if a Wayland compositor is running in this
/// process. The recorder uses it to attach to (or start) a capture without any Python client.
pub(crate) fn wayland_command_sender()
-> Option<smithay::reexports::calloop::channel::Sender<ThreadCommand>> {
    WAYLAND_BACKEND
        .lock()
        .unwrap()
        .as_ref()
        .map(|be| be.tx.clone())
}

/// Start the CU server if `PIXELFLUX_CU` names a bind (the standalone fallback;
/// a selkies-managed session passes the setting through [`start_cu_server`]).
pub fn spawn_cu_from_env() {
    if let Ok(bind) = std::env::var("PIXELFLUX_CU")
        && let Err(e) = start_cu_server(&bind, None)
    {
        println!("[ComputerUse] Not started: {e}");
    }
}

/// The token every request has to carry, fixed when the server starts.
static CU_TOKEN: OnceLock<String> = OnceLock::new();

/// Start the CU server on `bind`, comma-separated entries: a bare port listens on
/// the loopback addresses only, `host:port` names the address to listen on
/// (`0.0.0.0:port,[::]:port` accepts every interface), requiring `token`, else
/// `PIXELFLUX_CU_TOKEN`, of every request. Guarded so that exactly one server
/// binds per process no matter how many call sites (module import, Wayland
/// compositor init, the selkies setting) race to spawn it; backend selection stays
/// per-request, so a server bound at import serves a compositor that only starts
/// later.
pub fn start_cu_server(bind: &str, token: Option<&str>) -> Result<(), String> {
    static SPAWNED: OnceLock<()> = OnceLock::new();
    if SPAWNED.get().is_some() {
        return Ok(());
    }
    let token = token
        .map(str::to_string)
        .or_else(|| std::env::var("PIXELFLUX_CU_TOKEN").ok())
        .filter(|t| !t.is_empty())
        .ok_or("no token: set PIXELFLUX_CU_TOKEN, the bearer token every request has to carry")?;
    let listeners = cu_listeners(bind)?;
    if SPAWNED.set(()).is_err() {
        return Ok(());
    }
    let _ = CU_TOKEN.set(token);
    for listener in listeners {
        thread::spawn(move || run_cu_server(listener));
    }
    Ok(())
}

/// Whether an `Authorization` value carries the server's token, compared in constant time.
fn authorized(authorization: Option<&str>) -> bool {
    let Some(token) = CU_TOKEN.get() else {
        return false;
    };
    authorization
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|given| {
            given.len() == token.len()
                && given
                    .bytes()
                    .zip(token.bytes())
                    .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                    == 0
        })
}

/// Bound listeners for a CU bind: for each comma-separated entry, both loopback
/// addresses for a bare port and every address the host resolves to for
/// `host:port`. An address the host lacks (the IPv6 loopback with IPv6 disabled)
/// is skipped while another binds; any other failure, or nothing binding at all,
/// is an error.
fn cu_listeners(bind: &str) -> Result<Vec<TcpListener>, String> {
    let mut addrs: Vec<SocketAddr> = Vec::new();
    for entry in bind
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
    {
        let resolved: Vec<SocketAddr> = match entry.parse::<u16>() {
            Ok(port) => vec![
                SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
                SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
            ],
            Err(_) if entry.contains(':') => entry
                .to_socket_addrs()
                .map_err(|e| format!("cannot resolve '{entry}': {e}"))?
                .collect(),
            Err(_) => {
                return Err(format!(
                    "invalid bind '{entry}': expected a port or host:port"
                ));
            }
        };
        for addr in resolved {
            if !addrs.contains(&addr) {
                addrs.push(addr);
            }
        }
    }
    if addrs.is_empty() {
        return Err(format!(
            "invalid bind '{bind}': expected a port or host:port"
        ));
    }
    let mut listeners = Vec::new();
    let mut skipped = Vec::new();
    for addr in addrs {
        match bind_listener(addr) {
            Ok(listener) => listeners.push(listener),
            Err(e)
                if matches!(
                    e.raw_os_error(),
                    Some(libc::EADDRNOTAVAIL | libc::EAFNOSUPPORT)
                ) =>
            {
                skipped.push(format!("{addr} ({e})"));
            }
            Err(e) => return Err(format!("cannot bind {addr}: {e}")),
        }
    }
    if listeners.is_empty() {
        return Err(format!(
            "no address of '{bind}' is available: {}",
            skipped.join(", ")
        ));
    }
    for note in skipped {
        println!("[ComputerUse] Not listening on {note}");
    }
    Ok(listeners)
}

/// A listening socket on `addr`, IPv6-only for an IPv6 address so `[::]` and
/// `0.0.0.0` can share a port: the standard library binds `[::]` dual-stack and
/// offers no way to change that before the bind.
fn bind_listener(addr: SocketAddr) -> std::io::Result<TcpListener> {
    use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};

    let family = if addr.is_ipv4() {
        libc::AF_INET
    } else {
        libc::AF_INET6
    };
    let fd = unsafe { libc::socket(family, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let sock = unsafe { OwnedFd::from_raw_fd(fd) };
    let one: libc::c_int = 1;
    let enable = |level: libc::c_int, name: libc::c_int| -> std::io::Result<()> {
        let rc = unsafe {
            libc::setsockopt(
                fd,
                level,
                name,
                &one as *const libc::c_int as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    };
    enable(libc::SOL_SOCKET, libc::SO_REUSEADDR)?;
    if addr.is_ipv6() {
        enable(libc::IPPROTO_IPV6, libc::IPV6_V6ONLY)?;
    }
    let rc = match addr {
        SocketAddr::V4(v4) => {
            let mut sin: libc::sockaddr_in = unsafe { std::mem::zeroed() };
            sin.sin_family = libc::AF_INET as libc::sa_family_t;
            sin.sin_port = v4.port().to_be();
            sin.sin_addr = libc::in_addr {
                s_addr: u32::from_ne_bytes(v4.ip().octets()),
            };
            unsafe {
                libc::bind(
                    fd,
                    &sin as *const libc::sockaddr_in as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                )
            }
        }
        SocketAddr::V6(v6) => {
            let mut sin6: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
            sin6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            sin6.sin6_port = v6.port().to_be();
            sin6.sin6_flowinfo = v6.flowinfo();
            sin6.sin6_addr = libc::in6_addr {
                s6_addr: v6.ip().octets(),
            };
            sin6.sin6_scope_id = v6.scope_id();
            unsafe {
                libc::bind(
                    fd,
                    &sin6 as *const libc::sockaddr_in6 as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                )
            }
        }
    };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::listen(fd, 128) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { TcpListener::from_raw_fd(sock.into_raw_fd()) })
}

/// Pick the backend for one request: the registered Wayland compositor when present,
/// otherwise a fresh private connection to `DISPLAY`. The X11 connection is per-request so a
/// restarted X server never leaves the CU thread holding a dead connection.
pub(crate) fn resolve_backend() -> Result<Box<dyn CuBackend>, String> {
    if let Some(backend) = WAYLAND_BACKEND.lock().unwrap().clone() {
        return Ok(Box::new(backend));
    }
    crate::x11::computer_use::CuX11Backend::connect().map(|be| Box::new(be) as Box<dyn CuBackend>)
}

/// Expose the captured desktop to an AI agent over HTTP, so a Computer Use client can drive
/// the session much as a human viewer would.
///
/// It runs on its own thread serving `listener` POST `/computer-use` JSON actions
/// (screenshot, mouse_move, click, key, scroll, …). The backend and the framebuffer dimensions
/// are re-resolved on every request rather than cached, because the compositor can start, the
/// stream can resize, or the X server can restart underneath the agent — a stale size would
/// misplace every coordinate. On Wayland a screenshot forces a one-frame GPU readback when the
/// pipeline is in zero-copy mode; on X11 it is a one-shot `GetImage` of the root window.
pub fn run_cu_server(listener: TcpListener) {
    let addr = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_default();
    println!("[ComputerUse] Server listening on {addr}");
    // Requests are read on threads of their own, bounded in count, size, and time before
    // the token is checked, and acted on here one at a time.
    let (tx, rx) = mpsc::sync_channel::<(TcpStream, String, String)>(MAX_CU_PENDING);
    let pending = Arc::new(AtomicUsize::new(0));
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if pending.fetch_add(1, Ordering::AcqRel) >= MAX_CU_PENDING {
                pending.fetch_sub(1, Ordering::AcqRel);
                continue;
            }
            let (tx, pending) = (tx.clone(), pending.clone());
            thread::spawn(move || {
                if let Some(request) = read_cu_request(stream) {
                    let _ = tx.send(request);
                }
                pending.fetch_sub(1, Ordering::AcqRel);
            });
        }
    });

    let mut last_backend = "";
    for (mut stream, url, body) in rx {
        if let Some(json) = handle_record_endpoint(&url, &body) {
            cu_respond(&mut stream, 200, &json);
            continue;
        }

        let parsed: CuActionRequest = match serde_json::from_str(&body) {
            Ok(r) => r,
            Err(e) => {
                cu_respond(
                    &mut stream,
                    400,
                    &format!("{{\"error\":\"Invalid JSON: {}\"}}", e),
                );
                continue;
            }
        };

        let json_response = match resolve_backend() {
            Ok(backend) => {
                if backend.name() != last_backend {
                    last_backend = backend.name();
                    println!("[ComputerUse] Using {} backend", last_backend);
                }
                handle_action(parsed, backend.as_ref())
            }
            Err(e) => format!("{{\"error\":\"{}\"}}", e.replace('"', "\\\"")),
        };
        cu_respond(&mut stream, 200, &json_response);
    }
}

/// The most a CU request's line and headers may take; its body is one command.
const MAX_CU_HEAD: usize = 16 * 1024;
const MAX_CU_BODY: usize = 4 * 1024 * 1024;
/// Connections whose requests are being read or wait to be acted on; more are closed.
const MAX_CU_PENDING: usize = 16;
/// How long a connection has to send its request.
const CU_READ_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a refused request's connection is read on after its answer (`cu_refuse`).
const CU_LINGER: Duration = Duration::from_secs(1);

/// Read one HTTP/1.1 request: `(stream, path, body)` for an authorized one; anything
/// else is answered here (401, 4xx) or dropped, before more than its head is read.
fn read_cu_request(mut stream: TcpStream) -> Option<(TcpStream, String, String)> {
    stream.set_read_timeout(Some(CU_READ_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(CU_READ_TIMEOUT)).ok()?;
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        if buf.len() > MAX_CU_HEAD {
            cu_refuse(&mut stream, 431, "{\"error\":\"request head too large\"}");
            return None;
        }
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = std::str::from_utf8(&buf[..head_end]).ok()?;
    let mut lines = head.split("\r\n");
    let path = lines.next()?.split(' ').nth(1)?.to_string();
    let (mut authorization, mut length, mut chunked) = (None, Some(0usize), false);
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("authorization") {
            authorization = Some(value);
        } else if name.eq_ignore_ascii_case("content-length") {
            length = value.parse().ok();
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = true;
        }
    }
    if !authorized(authorization) {
        cu_refuse(&mut stream, 401, "{\"error\":\"unauthorized\"}");
        return None;
    }
    let Some(length) = length.filter(|_| !chunked) else {
        cu_refuse(
            &mut stream,
            411,
            "{\"error\":\"a Content-Length is required\"}",
        );
        return None;
    };
    if length > MAX_CU_BODY {
        cu_refuse(&mut stream, 413, "{\"error\":\"request body too large\"}");
        return None;
    }
    let mut body = buf.split_off(head_end + 4);
    body.truncate(length);
    if body.len() < length {
        let have = body.len();
        body.resize(length, 0);
        stream.read_exact(&mut body[have..]).ok()?;
    }
    match String::from_utf8(body) {
        Ok(body) => Some((stream, path, body)),
        Err(_) => {
            cu_refuse(&mut stream, 400, "{\"error\":\"body is not UTF-8\"}");
            None
        }
    }
}

/// Answer a request with a JSON body and close the connection.
/// Answer a request refused before all of it was read, then read on and drop what the client
/// still sends, for `CU_LINGER` and `MAX_CU_BODY` at most, before the connection is dropped: a
/// socket closed with unread data resets the connection, which can cut the answer short.
fn cu_refuse(stream: &mut TcpStream, status: u16, json: &str) {
    cu_respond(stream, status, json);
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let _ = stream.set_read_timeout(Some(CU_LINGER));
    let deadline = std::time::Instant::now() + CU_LINGER;
    let (mut sink, mut drained) = ([0u8; 4096], 0usize);
    while drained < MAX_CU_BODY && std::time::Instant::now() < deadline {
        match stream.read(&mut sink) {
            Ok(0) | Err(_) => break,
            Ok(n) => drained += n,
        }
    }
}

fn cu_respond(stream: &mut TcpStream, status: u16, json: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        411 => "Length Required",
        413 => "Payload Too Large",
        _ => "Request Header Fields Too Large",
    };
    let challenge = if status == 401 {
        "WWW-Authenticate: Bearer\r\n"
    } else {
        ""
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{challenge}Connection: close\r\n\r\n{json}",
        json.len()
    );
    let _ = stream.flush();
}

#[cfg(test)]
mod request_tests {
    use super::{CU_TOKEN, MAX_CU_BODY, read_cu_request};
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    /// Send `raw` as a client, read it as the server does: what the reader returned and
    /// what the client was answered.
    fn exchange(raw: Vec<u8>) -> (Option<(String, String)>, String) {
        let token = CU_TOKEN.get_or_init(|| "cu-test-token".into()).clone();
        let raw = String::from_utf8_lossy(&raw)
            .replace("TOKEN", &token)
            .into_bytes();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || {
            let mut c = TcpStream::connect(addr).unwrap();
            let _ = c.write_all(&raw);
            let _ = c.shutdown(std::net::Shutdown::Write);
            let mut answer = String::new();
            let _ = c.read_to_string(&mut answer);
            answer
        });
        let (stream, _) = listener.accept().unwrap();
        let got = read_cu_request(stream).map(|(_, path, body)| (path, body));
        (got, client.join().unwrap())
    }

    /// A request is read only so far as the bounds allow, and only an authorized one past
    /// its head: a huge header line, a missing token, and an oversized body are answered
    /// without the server reading on.
    #[test]
    fn a_request_is_bounded_and_authorized_before_its_body_is_read() {
        let body = r#"{"action":"screenshot"}"#;
        let (got, _) = exchange(
            format!("POST /computer-use HTTP/1.1\r\nAuthorization: Bearer TOKEN\r\nContent-Length: {}\r\n\r\n{body}", body.len())
                .into_bytes(),
        );
        assert_eq!(got, Some(("/computer-use".into(), body.into())));

        let mut huge = b"POST /computer-use HTTP/1.1\r\nX-Pad: ".to_vec();
        huge.resize(huge.len() + (1 << 20), b'a');
        let (got, answer) = exchange(huge);
        assert!(
            got.is_none() && answer.starts_with("HTTP/1.1 431"),
            "{answer}"
        );

        let (got, answer) =
            exchange(b"POST /computer-use HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}".to_vec());
        assert!(
            got.is_none() && answer.starts_with("HTTP/1.1 401"),
            "{answer}"
        );
        assert!(answer.contains("WWW-Authenticate: Bearer"), "{answer}");

        let (got, answer) = exchange(
            format!("POST /computer-use HTTP/1.1\r\nAuthorization: Bearer TOKEN\r\nContent-Length: {}\r\n\r\n", MAX_CU_BODY + 1)
                .into_bytes(),
        );
        assert!(
            got.is_none() && answer.starts_with("HTTP/1.1 413"),
            "{answer}"
        );

        let (got, answer) = exchange(
            b"POST /computer-use HTTP/1.1\r\nAuthorization: Bearer TOKEN\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n".to_vec(),
        );
        assert!(
            got.is_none() && answer.starts_with("HTTP/1.1 411"),
            "{answer}"
        );
    }
}

#[cfg(test)]
mod run_tests {
    use super::distinct_runs;

    /// A text is cut where a run would name one distinct character too many, and a
    /// character repeated in a run does not count again.
    #[test]
    fn a_text_is_typed_in_runs_of_few_distinct_characters() {
        let syms = [1, 2, 1, 3, 4, 4, 5, 1, 6];
        assert_eq!(
            distinct_runs(&syms, 3),
            vec![&[1, 2, 1, 3][..], &[4, 4, 5, 1][..], &[6][..]]
        );
        assert_eq!(distinct_runs(&syms, 9), vec![&syms[..]]);
        assert!(distinct_runs(&[], 3).is_empty());
    }
}

#[cfg(test)]
mod record_path_tests {
    use super::named_record_path;

    #[test]
    fn a_named_recording_stays_in_the_operators_directory() {
        assert_eq!(
            named_record_path(Some("/rec"), "take.mp4").unwrap(),
            "/rec/take.mp4"
        );
        for name in ["/etc/passwd", "../x.mp4", "a/b.mp4", "..", ".", ""] {
            assert!(named_record_path(Some("/rec"), name).is_err(), "{name:?}");
        }
        assert!(
            named_record_path(None, "take.mp4").is_err(),
            "no directory, no named file"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{
        GpuReadback, ReadbackSlot, ScreenshotAdmission, ScreenshotBuffers, ScreenshotFrame,
        ScreenshotPixelFormat, ScreenshotPixels, cu_listeners, encode_png_rgba,
    };
    use std::io::Cursor;
    use std::net::{IpAddr, Ipv4Addr, TcpListener};
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    #[test]
    fn screenshot_png_preserves_channels_alpha_and_encoding() {
        let mut rgba: Vec<u8> = (0..4 * 17 * 11).map(|n| (n * 37) as u8).collect();
        rgba[3] = 0;
        rgba[7] = 128;
        rgba[11] = 255;
        let mut previous_png = Vec::new();
        image::ImageBuffer::<image::Rgba<u8>, _>::from_raw(17, 11, rgba.clone())
            .unwrap()
            .write_to(&mut Cursor::new(&mut previous_png), image::ImageFormat::Png)
            .unwrap();
        assert_eq!(encode_png_rgba(&rgba, 17, 11).unwrap(), previous_png);
        for format in [ScreenshotPixelFormat::Rgba, ScreenshotPixelFormat::Bgra] {
            let mut pixels = rgba.clone();
            if matches!(format, ScreenshotPixelFormat::Bgra) {
                for px in pixels.as_chunks_mut::<4>().0 {
                    px.swap(0, 2);
                }
            }
            let png = ScreenshotFrame {
                pixels: pixels.into(),
                width: 17,
                height: 11,
                format,
            }
            .encode_png()
            .unwrap();
            assert_eq!(png, previous_png);
            let decoded = image::load_from_memory(&png).unwrap().into_rgba8();
            assert_eq!(decoded.dimensions(), (17, 11));
            assert_eq!(decoded.into_raw(), rgba);
        }
    }

    #[test]
    fn screenshot_rejects_incomplete_oversized_and_overflowing_frames() {
        for (width, height, len) in [
            (0, 1, 0),
            (1, 0, 0),
            (2, 2, 15),
            (2, 2, 17),
            (u32::MAX, u32::MAX, 0),
        ] {
            assert!(
                ScreenshotFrame {
                    pixels: vec![0; len].into(),
                    width,
                    height,
                    format: ScreenshotPixelFormat::Rgba,
                }
                .encode_png()
                .is_err()
            );
        }
    }

    #[test]
    fn screenshot_opaque_formats_ignore_unused_alpha_bytes() {
        for format in [ScreenshotPixelFormat::Rgbx, ScreenshotPixelFormat::Bgrx] {
            let mut pixels = vec![11, 22, 33, 0, 44, 55, 66, 128, 77, 88, 99, 255];
            if format == ScreenshotPixelFormat::Bgrx {
                for px in pixels.as_chunks_mut::<4>().0 {
                    px.swap(0, 2);
                }
            }
            let png = ScreenshotFrame {
                pixels: pixels.into(),
                width: 3,
                height: 1,
                format,
            }
            .encode_png()
            .unwrap();
            let decoded = image::load_from_memory(&png).unwrap().into_rgba8();
            assert_eq!(
                decoded.into_raw(),
                [11, 22, 33, 255, 44, 55, 66, 255, 77, 88, 99, 255]
            );
        }
    }

    /// A fence the test signals by hand.
    #[derive(Debug, Default)]
    struct TestFence(std::sync::atomic::AtomicBool);

    impl smithay::backend::renderer::sync::Fence for TestFence {
        fn is_signaled(&self) -> bool {
            self.0.load(Ordering::Acquire)
        }
        fn wait(&self) -> Result<(), smithay::backend::renderer::sync::Interrupted> {
            Ok(())
        }
        fn is_exportable(&self) -> bool {
            false
        }
        fn export(&self) -> Option<std::os::fd::OwnedFd> {
            None
        }
    }

    #[test]
    fn screenshot_readback_slot_copies_while_mapped_and_refuses_after() {
        let mapping: Vec<u8> = (0..16).collect();
        // SAFETY: `mapping` outlives every copy below.
        let slot = Arc::new(unsafe { ReadbackSlot::new(7, mapping.as_ptr(), mapping.len()) });
        assert!(!slot.in_use());
        let held = Arc::clone(&slot);
        assert!(slot.in_use());
        assert_eq!(&*held.copy_out(8).unwrap(), &mapping[..8]);
        assert!(held.copy_out(17).is_err());
        slot.unmap();
        assert_eq!(
            held.copy_out(8).err().unwrap(),
            "Screenshot compositor stopped"
        );
        drop(held);
        assert!(!slot.in_use());
    }

    #[test]
    fn screenshot_gpu_readback_waits_for_its_fence_until_the_deadline() {
        let mapping = [9u8; 12];
        // SAFETY: `mapping` outlives the readbacks below.
        let slot = Arc::new(unsafe { ReadbackSlot::new(1, mapping.as_ptr(), mapping.len()) });
        let fence = Arc::new(TestFence::default());
        let pending = |fence: &Arc<TestFence>| GpuReadback {
            slot: Arc::clone(&slot),
            sync: smithay::backend::renderer::sync::SyncPoint::from(TestFenceRef(Arc::clone(
                fence,
            ))),
            len: 12,
        };
        let late = pending(&fence).resolve(Instant::now() + Duration::from_millis(5));
        assert_eq!(late.err().unwrap(), "Screenshot timed out");
        assert!(!slot.in_use(), "a timed-out readback releases its slot");
        fence.0.store(true, Ordering::Release);
        let ready = pending(&fence)
            .resolve(Instant::now() + Duration::from_secs(1))
            .unwrap();
        assert_eq!(&*ready, &mapping[..]);
        let frame = ScreenshotFrame {
            pixels: ScreenshotPixels::Gpu(pending(&fence)),
            width: 3,
            height: 1,
            format: ScreenshotPixelFormat::Rgba,
        };
        assert!(
            ScreenshotFrame {
                pixels: ScreenshotPixels::Gpu(pending(&fence)),
                ..frame.resolve(Instant::now()).unwrap()
            }
            .encode_png()
            .is_err(),
            "pixels still on their way are never encoded"
        );
    }

    /// Shares a [`TestFence`] with the sync point that takes it.
    #[derive(Debug)]
    struct TestFenceRef(Arc<TestFence>);

    impl smithay::backend::renderer::sync::Fence for TestFenceRef {
        fn is_signaled(&self) -> bool {
            self.0.is_signaled()
        }
        fn wait(&self) -> Result<(), smithay::backend::renderer::sync::Interrupted> {
            Ok(())
        }
        fn is_exportable(&self) -> bool {
            false
        }
        fn export(&self) -> Option<std::os::fd::OwnedFd> {
            None
        }
    }

    #[test]
    fn screenshot_buffers_are_kept_for_the_next_screenshot_and_bounded() {
        static BUFFERS: ScreenshotBuffers = ScreenshotBuffers::new();
        let first = BUFFERS.take(64);
        assert_eq!(first.len(), 64);
        let address = first.as_ptr();
        drop(first);
        // The kept buffer comes back, cut to a smaller frame or grown for a larger one.
        let again = BUFFERS.take(16);
        assert_eq!((again.len(), again.as_ptr()), (16, address));
        drop(again);
        let grown = BUFFERS.take(4096);
        assert_eq!(grown.len(), 4096);
        let held: Vec<_> = (0..4).map(|_| BUFFERS.take(8)).collect();
        drop((grown, held));
        assert_eq!(BUFFERS.kept.lock().unwrap().len(), 2);
    }

    #[test]
    fn screenshot_capacity_spans_readback_and_encoding_until_release() {
        let admission = ScreenshotAdmission::default();
        let first = admission.acquire(0).unwrap();
        let canceled = Arc::clone(&first.canceled);
        let second = admission.acquire(0).unwrap();
        assert!(admission.acquire(0).is_err());
        let third = admission.acquire(1).unwrap();
        let fourth = admission.acquire(2).unwrap();
        assert!(admission.acquire(3).is_err());
        assert!(!canceled.load(Ordering::Acquire));
        drop(first);
        assert!(canceled.load(Ordering::Acquire));
        let replacement = admission.acquire(3).unwrap();
        assert!(admission.acquire(0).is_err());
        drop((second, third, fourth, replacement));
        assert!(admission.counts.lock().unwrap().is_empty());
        assert!(admission.acquire(0).is_ok());
    }

    #[test]
    fn bare_port_binds_loopback_only() {
        let listeners = cu_listeners("0").expect("loopback binds");
        assert!(!listeners.is_empty());
        for listener in &listeners {
            let addr = listener.local_addr().unwrap();
            assert!(addr.ip().is_loopback(), "{addr}");
            assert_ne!(addr.port(), 0);
        }
        assert!(
            listeners
                .iter()
                .any(|l| l.local_addr().unwrap().ip() == IpAddr::V4(Ipv4Addr::LOCALHOST))
        );
    }

    #[test]
    fn host_port_names_the_address() {
        let scoped = cu_listeners("127.0.0.1:0").unwrap();
        assert_eq!(scoped.len(), 1);
        assert_eq!(
            scoped[0].local_addr().unwrap().ip(),
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        );
        let open = cu_listeners("0.0.0.0:0").unwrap();
        assert_eq!(open.len(), 1);
        assert!(open[0].local_addr().unwrap().ip().is_unspecified());
    }

    #[test]
    fn a_taken_port_is_an_error() {
        let taken = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = taken.local_addr().unwrap().port();
        assert!(cu_listeners(&port.to_string()).is_err());
        assert!(cu_listeners(&format!("127.0.0.1:{port}")).is_err());
    }

    #[test]
    fn malformed_binds_are_errors() {
        assert!(cu_listeners("abc").is_err());
        assert!(cu_listeners("70000").is_err());
        assert!(cu_listeners("").is_err());
        assert!(cu_listeners(",").is_err());
        assert!(cu_listeners("127.0.0.1:0,abc").is_err());
    }

    fn free_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    fn v6only(listener: &TcpListener) -> libc::c_int {
        use std::os::fd::AsRawFd;
        let mut value: libc::c_int = -1;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(
                listener.as_raw_fd(),
                libc::IPPROTO_IPV6,
                libc::IPV6_V6ONLY,
                &mut value as *mut libc::c_int as *mut libc::c_void,
                &mut len,
            )
        };
        assert_eq!(rc, 0);
        value
    }

    #[test]
    fn both_wildcards_share_a_port() {
        let port = free_port();
        let listeners = cu_listeners(&format!("0.0.0.0:{port}, [::]:{port}")).unwrap();
        assert!(
            listeners
                .iter()
                .all(|l| l.local_addr().unwrap().port() == port)
        );
        assert!(
            listeners
                .iter()
                .all(|l| l.local_addr().unwrap().ip().is_unspecified())
        );
        if let Some(v6) = listeners.iter().find(|l| l.local_addr().unwrap().is_ipv6()) {
            assert_eq!(listeners.len(), 2);
            assert_eq!(v6only(v6), 1);
        }
    }

    #[test]
    fn repeated_entries_bind_once() {
        let port = free_port();
        let listeners = cu_listeners(&format!("{port},127.0.0.1:{port},{port}")).unwrap();
        assert!(listeners.len() <= 2);
        assert!(
            listeners
                .iter()
                .all(|l| l.local_addr().unwrap().ip().is_loopback())
        );
    }
}
