/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Out-of-band X11 cursor delivery, the X11 counterpart of the Wayland compositor's cursor
//! callback: one process-wide monitor thread (the X pointer is global, however many captures
//! run) parks in `wait_for_event` on its own connection for XFixes `DisplayCursorNotify` and
//! hands each new cursor to the registered `CursorSink` — the same payload the Wayland
//! backend sends, so a consumer needs one handler for both.
//! Keeping the cursor out of the framebuffer means pointer motion over static content never
//! dirties the damage hash or re-encodes video; `capture_cursor` compositing stays available
//! as the opt-in alternative.
//!
//! The thread blocks — no polling. Stop and replay requests wake it by sending a
//! ClientMessage to a private InputOnly window from a short-lived connection, which also
//! keeps every request on the monitor's own connection single-threaded. The SeqCst pairs on
//! `stop`/`wake_win` close the startup race where a stop lands before the wake window
//! exists: whichever side loses the ordering still observes the other's store.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::sink::{CursorImage, CursorSink};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xfixes::{ConnectionExt as XfixesExt, CursorNotifyMask, GetCursorImageReply};
use x11rb::protocol::xproto::{
    CLIENT_MESSAGE_EVENT, ClientMessageEvent, ConnectionExt as XprotoExt, CreateWindowAux,
    EventMask, WindowClass,
};
use x11rb::rust_connection::RustConnection;

/// The cursor sink from `set_cursor_callback`, shared by every X11 capture.
static CALLBACK: Mutex<Option<Arc<dyn CursorSink>>> = Mutex::new(None);
/// Longest delivered cursor edge; larger images are downscaled (`<= 0` = uncapped).
/// 128 is what a browser shows as a cursor, so an application's own sprite travels whole.
static SIZE_CAP: AtomicI32 = AtomicI32::new(128);
/// Deliver the current cursor on the next wake (a callback registered mid-run).
static REPLAY: AtomicBool = AtomicBool::new(false);

struct Monitor {
    stop: Arc<AtomicBool>,
    wake_win: Arc<AtomicU32>,
    /// Closed (by drop) when the thread exits; lets `release` bound its join.
    done_rx: std::sync::mpsc::Receiver<()>,
    join: JoinHandle<()>,
}

/// The monitor and the number of running X11 captures that keep it alive.
struct Slot {
    users: usize,
    monitor: Option<Monitor>,
}

static SLOT: Mutex<Slot> = Mutex::new(Slot {
    users: 0,
    monitor: None,
});

/// Register/replace the callback and re-deliver the current cursor to it. `None` withdraws
/// it, releasing what the callback holds: the slot is process-wide and outlives the capture
/// that set it, so a consumer that is going away has no other way to let go.
pub fn set_callback(cb: Option<Arc<dyn CursorSink>>) {
    let registered = cb.is_some();
    *CALLBACK.lock().unwrap() = cb;
    if !registered {
        return;
    }
    let slot = SLOT.lock().unwrap();
    if let Some(m) = slot.monitor.as_ref() {
        REPLAY.store(true, Ordering::Release);
        wake(&m.wake_win);
    }
}

pub fn set_size_cap(cap: i32) {
    SIZE_CAP.store(cap, Ordering::Relaxed);
}

/// An X11 capture started: the first one in spawns the monitor.
pub fn acquire() {
    let mut slot = SLOT.lock().unwrap();
    slot.users += 1;
    if slot.monitor.is_none() {
        let stop = Arc::new(AtomicBool::new(false));
        let wake_win = Arc::new(AtomicU32::new(0));
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let (tstop, twin) = (stop.clone(), wake_win.clone());
        match std::thread::Builder::new()
            .name("pxf-x11-cursor".into())
            .spawn(move || {
                let _done = done_tx;
                monitor_thread(tstop, twin);
            }) {
            Ok(join) => {
                slot.monitor = Some(Monitor {
                    stop,
                    wake_win,
                    done_rx,
                    join,
                })
            }
            Err(e) => eprintln!("[X11] cursor monitor spawn failed: {e}"),
        }
    }
}

/// An X11 capture stopped: the last one out stops and joins the monitor. A caller holding
/// the GIL runs it detached — the thread may be blocked delivering a cursor to a sink that
/// needs it — and the join is bounded: a thread that misses the wake (X server wedged
/// mid-shutdown) is abandoned with a warning rather than hanging the caller; it exits on its
/// own next event. A stop issued from inside the cursor sink runs on the monitor thread
/// itself, where a join can only time out: it signals and detaches instead, and the loop
/// observes the stop flag once the sink returns.
pub fn release() {
    let monitor = {
        let mut slot = SLOT.lock().unwrap();
        slot.users = slot.users.saturating_sub(1);
        if slot.users == 0 {
            slot.monitor.take()
        } else {
            None
        }
    };
    if let Some(m) = monitor {
        if m.join.thread().id() == std::thread::current().id() {
            m.stop.store(true, Ordering::SeqCst);
            wake(&m.wake_win);
            return;
        }
        m.stop.store(true, Ordering::SeqCst);
        wake(&m.wake_win);
        match m.done_rx.recv_timeout(std::time::Duration::from_secs(2)) {
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                eprintln!("[X11] cursor monitor did not stop in time; detaching");
            }
            _ => {
                let _ = m.join.join();
            }
        }
    }
}

/// Consumer teardown: drop the sink and signal the thread without joining — `SHUTDOWN`
/// already gates it off the consumer, and a normal capture stop reaps it via `release`.
pub fn shutdown() {
    *CALLBACK.lock().unwrap() = None;
    let slot = SLOT.lock().unwrap();
    if let Some(m) = slot.monitor.as_ref() {
        m.stop.store(true, Ordering::SeqCst);
        wake(&m.wake_win);
    }
}

/// Wake the parked monitor: a ClientMessage to its private window (SendEvent with an empty
/// mask delivers to the window's creator) from a short-lived connection. The send is
/// round-tripped (`check`) so a failure has a name, and the whole wake is retried once. A
/// zero window means setup hasn't finished; the thread re-checks its flags before first
/// parking, so the wake can be skipped.
fn wake(wake_win: &AtomicU32) {
    let win = wake_win.load(Ordering::SeqCst);
    if win == 0 {
        return;
    }
    let mut last_err = String::new();
    for attempt in 0..2 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let conn = match x11rb::connect(None) {
            Ok((c, _)) => c,
            Err(e) => {
                last_err = format!("connect: {e}");
                continue;
            }
        };
        let ev = ClientMessageEvent {
            response_type: CLIENT_MESSAGE_EVENT,
            format: 32,
            sequence: 0,
            window: win,
            type_: u32::from(x11rb::protocol::xproto::AtomEnum::PRIMARY),
            data: [0u32; 5].into(),
        };
        match conn
            .send_event(false, win, EventMask::NO_EVENT, ev)
            .map_err(|e| e.to_string())
            .and_then(|c| c.check().map_err(|e| e.to_string()))
        {
            Ok(()) => return,
            Err(e) => last_err = format!("send_event: {e}"),
        }
    }
    eprintln!("[X11] cursor monitor wake failed: {last_err}");
}

/// The last payload handed to the sink, retained so a replay (late callback registration)
/// re-sends it without touching the server — mirroring the Wayland backend, and immune to
/// XFixes' transient "cursor not displayed" fetch errors.
type Payload = (&'static str, Vec<u8>, i32, i32);

fn monitor_thread(stop: Arc<AtomicBool>, wake_win: Arc<AtomicU32>) {
    let (conn, screen_num) = match x11rb::connect(None) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[X11] cursor monitor: connect failed: {e}");
            return;
        }
    };
    let root = conn.setup().roots[screen_num].root;
    if let Err(e) = setup(&conn, root, &wake_win) {
        eprintln!("[X11] cursor monitor unavailable: {e}");
        return;
    }
    if stop.load(Ordering::SeqCst) {
        return;
    }
    let mut last: Option<Payload> = fetch_payload(&conn);
    deliver(last.as_ref());
    // A registration that raced setup may have requested a replay before the wake
    // window existed (its wake was skipped). The deliver above satisfied it only if
    // the callback was already visible and the fetch produced a payload, so consume
    // the flag and re-deliver rather than assume: an unconditional clear leaves that
    // callback cursor-less until the next real cursor change. From here on the wake
    // window exists, so later requests always reach the loop below.
    if REPLAY.swap(false, Ordering::AcqRel) {
        if last.is_none() {
            last = fetch_payload(&conn);
        }
        deliver(last.as_ref());
    }
    loop {
        let event = match conn.wait_for_event() {
            Ok(ev) => ev,
            Err(e) => {
                eprintln!("[X11] cursor monitor: connection lost: {e}");
                return;
            }
        };
        let mut changed = matches!(event, Event::XfixesCursorNotify(_));
        // Coalesce a queued burst into one fetch of the current image.
        while let Ok(Some(ev)) = conn.poll_for_event() {
            changed |= matches!(ev, Event::XfixesCursorNotify(_));
        }
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let replay = REPLAY.swap(false, Ordering::AcqRel);
        if changed {
            // A failed fetch keeps the retained payload (consumers keep the last
            // cursor); the change is dropped, matching the python monitor's skip.
            if let Some(p) = fetch_payload(&conn) {
                last = Some(p);
                deliver(last.as_ref());
            } else if replay {
                deliver(last.as_ref());
            }
        } else if replay {
            if last.is_none() {
                last = fetch_payload(&conn);
            }
            deliver(last.as_ref());
        }
    }
}

fn setup(conn: &RustConnection, root: u32, wake_win: &AtomicU32) -> Result<(), String> {
    conn.xfixes_query_version(5, 0)
        .map_err(|e| format!("xfixes_query_version: {e}"))?
        .reply()
        .map_err(|e| format!("XFixes unavailable: {e}"))?;
    conn.xfixes_select_cursor_input(root, CursorNotifyMask::DISPLAY_CURSOR)
        .map_err(|e| format!("select_cursor_input: {e}"))?
        .check()
        .map_err(|e| format!("select_cursor_input failed: {e}"))?;
    let win = conn
        .generate_id()
        .map_err(|e| format!("generate_id: {e}"))?;
    conn.create_window(
        0,
        win,
        root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        0,
        &CreateWindowAux::new(),
    )
    .map_err(|e| format!("create_window: {e}"))?
    .check()
    .map_err(|e| format!("wake window: {e}"))?;
    wake_win.store(win, Ordering::SeqCst);
    Ok(())
}

/// Fetch and convert the current cursor. `None` on a fetch failure (XFixes returns an
/// error while the cursor is not displayed, e.g. a blanked screen) or an encode failure —
/// mirroring the Wayland gate where only real payloads or an explicit hide go downstream.
fn fetch_payload(conn: &RustConnection) -> Option<Payload> {
    let img = conn.xfixes_get_cursor_image().ok()?.reply().ok()?;
    let (msg_type, png, hot_x, hot_y) = cursor_to_png(&img, SIZE_CAP.load(Ordering::Relaxed));
    if png.is_empty() && msg_type != "hide" {
        return None;
    }
    Some((msg_type, png, hot_x, hot_y))
}

/// Hand a payload to the sink (skips quietly with no sink, no payload, or a consumer that
/// is gone).
fn deliver(payload: Option<&Payload>) {
    let (msg_type, png, hot_x, hot_y) = match payload {
        Some(p) => p,
        None => return,
    };
    if crate::SHUTDOWN.load(Ordering::Relaxed) {
        return;
    }
    let cb = CALLBACK.lock().unwrap().clone();
    if let Some(cb) = cb {
        cb.cursor(CursorImage {
            kind: msg_type,
            png: png.clone(),
            hot_x: *hot_x,
            hot_y: *hot_y,
        });
    }
}

/// XFixes ARGB image -> callback payload: cropped to the visible bounding box (hotspot
/// re-based to it), downscaled so the longest edge fits `cap`, un-premultiplied, and
/// PNG-encoded. Scaling runs in premultiplied space — per-channel filtering is only linear
/// there, and fully transparent texels cannot bleed dark fringes into edges — and straight
/// alpha is produced last, since that is what PNG carries. A fully transparent image is an
/// intentional pointer hide.
fn cursor_to_png(img: &GetCursorImageReply, cap: i32) -> (&'static str, Vec<u8>, i32, i32) {
    let w = img.width as usize;
    let h = img.height as usize;
    if w == 0 || h == 0 || img.cursor_image.len() < w * h {
        return ("hide", Vec::new(), 0, 0);
    }
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0usize, 0usize);
    for y in 0..h {
        for x in 0..w {
            if img.cursor_image[y * w + x] != 0 {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
    }
    if x0 > x1 || y0 > y1 {
        return ("hide", Vec::new(), 0, 0);
    }
    let (cw, ch) = (x1 - x0 + 1, y1 - y0 + 1);
    let mut rgba = Vec::with_capacity(cw * ch * 4);
    for y in y0..=y1 {
        for x in x0..=x1 {
            let p = img.cursor_image[y * w + x];
            rgba.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, p as u8, (p >> 24) as u8]);
        }
    }
    let mut hot_x = img.xhot as i32 - x0 as i32;
    let mut hot_y = img.yhot as i32 - y0 as i32;
    let mut image = match image::RgbaImage::from_raw(cw as u32, ch as u32, rgba) {
        Some(i) => i,
        None => return ("error", Vec::new(), 0, 0),
    };
    if cap > 0 && (cw > cap as usize || ch > cap as usize) {
        let scale = cap as f32 / cw.max(ch) as f32;
        let nw = ((cw as f32 * scale) as u32).max(1);
        let nh = ((ch as f32 * scale) as u32).max(1);
        image = image::imageops::resize(&image, nw, nh, image::imageops::FilterType::Lanczos3);
        hot_x = (hot_x as f32 * scale) as i32;
        hot_y = (hot_y as f32 * scale) as i32;
    }
    crate::unpremultiply_rgba(&mut image);
    // A hotspot can lie outside the visible bbox (its neighborhood was cropped as
    // fully transparent). Consumers treat the hotspot as an offset INTO the
    // bitmap, so clamp to the cropped bounds instead of emitting off-image
    // coordinates.
    let hot_x = hot_x.clamp(0, image.width() as i32 - 1);
    let hot_y = hot_y.clamp(0, image.height() as i32 - 1);
    let mut png = Vec::new();
    match image.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png) {
        Ok(()) => ("png", png, hot_x, hot_y),
        Err(_) => ("error", Vec::new(), 0, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(w: u16, h: u16, xhot: u16, yhot: u16, pixels: Vec<u32>) -> GetCursorImageReply {
        GetCursorImageReply {
            sequence: 0,
            length: 0,
            x: 0,
            y: 0,
            width: w,
            height: h,
            xhot,
            yhot,
            cursor_serial: 1,
            cursor_image: pixels,
        }
    }

    /// A fully transparent cursor is an intentional hide, matching the python path's
    /// empty-bbox -> empty-curdata behavior that blanks the client cursor.
    #[test]
    fn transparent_cursor_hides() {
        let (t, data, _, _) = cursor_to_png(&reply(4, 4, 0, 0, vec![0; 16]), 32);
        assert_eq!(t, "hide");
        assert!(data.is_empty());
        let (t, _, _, _) = cursor_to_png(&reply(0, 0, 0, 0, vec![]), 32);
        assert_eq!(t, "hide");
    }

    /// The image is cropped to its visible bbox and the hotspot re-based to the crop:
    /// a 2x2 visible block at (1,1)..(2,2) with hotspot (2,2) yields a 2x2 PNG with
    /// hotspot (1,1).
    #[test]
    fn application_sprite_travels_whole_within_the_cap() {
        // A 128px sprite, a game's own pointer, at the default cap: delivered at 128.
        let opaque = vec![0xff00_0000u32; 128 * 128];
        let (t, data, _, _) = cursor_to_png(&reply(128, 128, 0, 0, opaque.clone()), 128);
        assert_eq!(t, "png");
        let img = image::load_from_memory(&data).unwrap();
        assert_eq!((img.width(), img.height()), (128, 128));
        // Past the cap, the longest edge is brought to it.
        let big = vec![0xff00_0000u32; 256 * 256];
        let (_, data, _, _) = cursor_to_png(&reply(256, 256, 0, 0, big), 128);
        let img = image::load_from_memory(&data).unwrap();
        assert_eq!((img.width(), img.height()), (128, 128));
    }

    #[test]
    fn crop_rebases_hotspot() {
        let mut px = vec![0u32; 16];
        for (x, y) in [(1, 1), (2, 1), (1, 2), (2, 2)] {
            px[y * 4 + x] = 0xFF00_0000;
        }
        let (t, data, hx, hy) = cursor_to_png(&reply(4, 4, 2, 2, px), 32);
        assert_eq!(t, "png");
        assert!(!data.is_empty());
        assert_eq!((hx, hy), (1, 1));
    }

    /// A hotspot whose neighborhood was cropped away as transparent is clamped into
    /// the emitted bitmap — consumers use it as an offset INTO the image, and the
    /// Wayland path never emits out-of-bounds hotspots either.
    #[test]
    fn out_of_bbox_hotspot_clamped() {
        let mut px = vec![0u32; 16];
        let (x, y) = (2usize, 1usize);
        px[y * 4 + x] = 0xFF00_0000;
        // Visible pixel at (2,1) only. Hotspot (0,0): rebased (-2,-1) -> (0,0).
        let (t, _, hx, hy) = cursor_to_png(&reply(4, 4, 0, 0, px.clone()), 32);
        assert_eq!(t, "png");
        assert_eq!((hx, hy), (0, 0));
        // Hotspot (3,3): rebased (1,2) past the 1x1 crop -> clamps to (0,0).
        let (_, _, hx, hy) = cursor_to_png(&reply(4, 4, 3, 3, px), 32);
        assert_eq!((hx, hy), (0, 0));
    }

    /// Premultiplied color becomes straight alpha in the PNG: a half-alpha pixel stored
    /// as ARGB (128,64,32,16) decodes to RGBA (128,64,32) at alpha 128. selkies'
    /// `unpremultiply_rgba` (display_utils) mirrors the same integer rounding so the seed
    /// and live paths hash a cursor to the same content handle.
    #[test]
    fn fractional_alpha_unpremultiplied() {
        let (t, data, _, _) = cursor_to_png(&reply(2, 2, 0, 0, vec![0x8040_2010; 4]), 32);
        assert_eq!(t, "png");
        let img = image::load_from_memory(&data).unwrap().to_rgba8();
        assert_eq!(img.get_pixel(0, 0).0, [128, 64, 32, 128]);
        // Binary alpha passes through untouched.
        let (_, data, _, _) = cursor_to_png(&reply(1, 1, 0, 0, vec![0xFF10_2030]), 32);
        let img = image::load_from_memory(&data).unwrap().to_rgba8();
        assert_eq!(img.get_pixel(0, 0).0, [0x10, 0x20, 0x30, 0xFF]);
    }

    /// An oversized cursor is downscaled so its longest edge fits the cap, with the
    /// hotspot scaled by the same factor.
    #[test]
    fn oversized_cursor_capped() {
        let (t, data, hx, hy) =
            cursor_to_png(&reply(64, 64, 32, 32, vec![0xFFFF_FFFF; 64 * 64]), 16);
        assert_eq!(t, "png");
        let img = image::load_from_memory(&data).unwrap();
        assert_eq!((img.width(), img.height()), (16, 16));
        assert_eq!((hx, hy), (8, 8));
        // Uncapped passes through at native size.
        let (_, data, _, _) = cursor_to_png(&reply(64, 64, 32, 32, vec![0xFFFF_FFFF; 64 * 64]), 0);
        let img = image::load_from_memory(&data).unwrap();
        assert_eq!((img.width(), img.height()), (64, 64));
    }
}
