/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! X11 backend for the Computer Use HTTP API: XTEST injection and one-shot root screenshots
//! over a private x11rb connection, so the agent can drive an X session with no active capture
//! and no shared state with any streaming capture thread (the X server itself serializes).
//!
//! Keysyms the active layout cannot produce (after base+AltGr resolution, which stays the
//! preferred path) are typed through a transient remap: under `XGrabServer` the keymap is
//! re-fetched, the needed keysyms are bound onto all-NoSymbol spare keycodes with ONE
//! `XChangeKeyboardMapping`, and the key sequence is injected; after a settle window (which
//! lets the focused client re-fetch the bound map — see `type_with_transient_binds`) one
//! conditional `XChangeKeyboardMapping` under a second grab puts the spares back — two
//! MappingNotify broadcasts per action. This coexists with a selkies session's own
//! spare-keycode overlay allocator on the same server: spares here are chosen DESCENDING
//! from the top of the keycode range while selkies allocates ASCENDING, each grab re-fetches
//! the keymap so every binding selkies already made is respected, the restore clears only
//! keycodes still carrying our content, and selkies' foreign-change invalidation (fired by
//! each MappingNotify) is self-healing because its held keys live on BOUND (non-NoSymbol)
//! keycodes an all-NoSymbol spare can never steal.

use std::cell::RefCell;
use std::collections::HashMap;
use std::thread;
use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::protocol::xfixes::ConnectionExt as XfixesExt;
use x11rb::protocol::xproto::{
    BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, ConnectionExt as XprotoExt, ImageFormat,
    KEY_PRESS_EVENT, KEY_RELEASE_EVENT, MOTION_NOTIFY_EVENT,
};
use x11rb::protocol::xtest::ConnectionExt as XtestExt;
use x11rb::rust_connection::RustConnection;

use crate::computer_use::{CuBackend, CuButton, encode_png_rgba};

/// One wheel "click" of scroll per unit of CU `scroll_amount`, capped so a hostile amount
/// cannot flood the server with press/release pairs.
const MAX_SCROLL_CLICKS: i32 = 100;

const KEYSYM_ISO_LEVEL3_SHIFT: u32 = 0xfe03;
const KEYSYM_MODE_SWITCH: u32 = 0xff7e;

/// Reverse view of the server's current keymap.
struct ServerKeymap {
    /// keysym -> (keycode, shift level); level bit 0 = Shift, bit 1 = AltGr, lowest
    /// level wins across ALL keys so a keysym reachable unshifted never resolves to a
    /// modified position.
    by_sym: HashMap<u32, (u32, u32)>,
    /// Keycode carrying `ISO_Level3_Shift` (or, failing that, `Mode_switch`) in the core
    /// map — the key synthesized around AltGr-level hits. 0 when the layout has neither,
    /// in which case no AltGr levels are offered at all.
    altgr_keycode: u32,
}

pub struct CuX11Backend {
    conn: RustConnection,
    root: u32,
    has_xfixes: bool,
    /// Reverse map of the SERVER keymap, built lazily from `GetKeyboardMapping` and kept
    /// for this backend's lifetime — one HTTP request (the backend is per-request), so a
    /// `setxkbmap` switch is seen by the next request.
    reverse_keymap: RefCell<Option<ServerKeymap>>,
}

impl CuX11Backend {
    /// Connect to the X server named by `DISPLAY` and negotiate XTEST (required for any
    /// injection). XFixes is optional and only gates cursor compositing in screenshots.
    pub fn connect() -> Result<Self, String> {
        let (conn, screen_num) =
            x11rb::connect(None).map_err(|e| format!("X11 connect failed: {e}"))?;
        let root = conn.setup().roots[screen_num].root;
        conn.xtest_get_version(2, 2)
            .map_err(|e| format!("xtest_get_version: {e}"))?
            .reply()
            .map_err(|e| format!("XTEST unavailable: {e}"))?;
        let has_xfixes = conn
            .xfixes_query_version(5, 0)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some();
        Ok(Self {
            conn,
            root,
            has_xfixes,
            reverse_keymap: RefCell::new(None),
        })
    }

    /// Build the reverse view of the server's current keymap from `GetKeyboardMapping`.
    ///
    /// Column -> (group, level) convention, as observed on XKB-compat servers (Xvfb/Xorg
    /// under `setxkbmap de`: keycode 24 = `q Q q Q at Greek_OMEGA at`): columns 0/1 are
    /// group-1 plain/Shift, columns 2/3 mirror them as core group 2, and columns 4/5
    /// carry group-1 levels 3/4 (AltGr, Shift+AltGr). Columns 4/5 are consulted only
    /// when the map carries a level-3 modifier key to synthesize; columns 2/3 (group 2)
    /// never are.
    fn build_reverse_keymap(&self) -> ServerKeymap {
        let mut km = ServerKeymap {
            by_sym: HashMap::new(),
            altgr_keycode: 0,
        };
        let setup = self.conn.setup();
        let (lo, hi) = (setup.min_keycode, setup.max_keycode);
        let Some(reply) = self
            .conn
            .get_keyboard_mapping(lo, hi - lo + 1)
            .ok()
            .and_then(|c| c.reply().ok())
        else {
            return km;
        };
        let per = reply.keysyms_per_keycode as usize;
        if per == 0 {
            return km;
        }
        let keycode_of = |wanted: u32| {
            reply
                .keysyms
                .chunks_exact(per)
                .position(|syms| syms.contains(&wanted))
                .map(|i| lo as u32 + i as u32)
        };
        km.altgr_keycode = keycode_of(KEYSYM_ISO_LEVEL3_SHIFT)
            .or_else(|| keycode_of(KEYSYM_MODE_SWITCH))
            .unwrap_or(0);
        // Ascending level order so lower levels win; level bit 0 = Shift, bit 1 = AltGr.
        let columns: [(usize, u32); 4] = [(0, 0), (1, 1), (4, 2), (5, 3)];
        for (col, level) in columns {
            if col >= per || (level & 2 != 0 && km.altgr_keycode == 0) {
                continue;
            }
            for (i, syms) in reply.keysyms.chunks_exact(per).enumerate() {
                let sym = syms[col];
                if sym != 0 {
                    km.by_sym
                        .entry(sym)
                        .or_insert((lo as u32 + i as u32, level));
                }
            }
        }
        km
    }

    /// Fire one XTEST fake event and flush so it reaches the server before the action
    /// layer's inter-event pacing sleep, matching the immediacy of real input.
    fn fake_input(&self, kind: u8, detail: u8, root: u32, x: i16, y: i16) {
        let _ = self
            .conn
            .xtest_fake_input(kind, detail, x11rb::CURRENT_TIME, root, x, y, 0)
            .map(|c| c.ignore_error());
        let _ = self.conn.flush();
    }

    fn root_geometry(&self) -> Result<(u16, u16), String> {
        let geo = self
            .conn
            .get_geometry(self.root)
            .map_err(|e| format!("get_geometry: {e}"))?
            .reply()
            .map_err(|e| format!("get_geometry reply: {e}"))?;
        Ok((geo.width, geo.height))
    }

    /// One server round trip, forcing everything already sent on this connection to be
    /// processed before the next request is issued (the XSync idiom).
    fn sync(&self) -> Result<(), String> {
        self.conn
            .get_input_focus()
            .map_err(|e| format!("sync: {e}"))?
            .reply()
            .map_err(|e| format!("sync reply: {e}"))?;
        Ok(())
    }

    /// Bind `keysyms` onto spare keycodes and run `seq` with the keysym -> keycode map,
    /// choose/bind/inject atomically under a server grab; then, after a settle window,
    /// restore the spares under a second grab. The settle exists because clients
    /// translate a keycode by RE-FETCHING the keymap when they process the bind's
    /// MappingNotify — a fetch the grab itself blocks — so a restore issued inside the
    /// first grab would be what they read back and every transient key would translate
    /// to nothing (observed with xterm). Once `seq` has run, every failure is logged and
    /// swallowed so the caller never re-runs the sequence.
    fn type_with_transient_binds(
        &self,
        keysyms: &[u32],
        seq: &mut dyn FnMut(&HashMap<u32, u32>),
    ) -> Result<(), String> {
        let setup = self.conn.setup();
        let (lo, hi) = (setup.min_keycode, setup.max_keycode);
        let mut chosen: Vec<u32> = Vec::with_capacity(keysyms.len());
        let (span_lo, count, per, bound_syms) = {
            self.conn
                .grab_server()
                .map_err(|e| format!("grab_server: {e}"))?;
            let _guard = ServerGrabGuard { conn: &self.conn };
            // Re-fetched UNDER the grab: a spare keycode any other client bound before
            // the grab is visible as bound here and never chosen.
            let reply = self
                .conn
                .get_keyboard_mapping(lo, hi - lo + 1)
                .map_err(|e| format!("get_keyboard_mapping: {e}"))?
                .reply()
                .map_err(|e| format!("get_keyboard_mapping reply: {e}"))?;
            let per = reply.keysyms_per_keycode as usize;
            if per == 0 {
                return Err("empty keymap".to_string());
            }
            // Spares are all-NoSymbol keycodes taken DESCENDING from the top of the
            // range. selkies' overlay allocator scans ASCENDING, so the two only meet
            // when nearly every spare on the server is taken; and any held key
            // (selkies' overlay binds included) lives on a BOUND, non-NoSymbol keycode,
            // so an all-NoSymbol spare can never steal a key that is currently down.
            for (i, syms) in reply.keysyms.chunks_exact(per).enumerate().rev() {
                if syms.iter().all(|&s| s == 0) {
                    chosen.push(lo as u32 + i as u32);
                    if chosen.len() == keysyms.len() {
                        break;
                    }
                }
            }
            if chosen.len() < keysyms.len() {
                return Err(format!(
                    "only {} spare keycodes for {} unresolved keysyms",
                    chosen.len(),
                    keysyms.len()
                ));
            }
            // ONE ChangeKeyboardMapping over the span from the lowest chosen spare
            // upward. Every all-NoSymbol keycode above the lowest chosen one was itself
            // chosen, so the span's other keycodes are bound ones, rewritten with their
            // existing content (a content no-op). Each transient key carries its keysym
            // at the plain and Shift levels so a stray held Shift cannot change what it
            // types.
            let span_lo = *chosen.last().unwrap();
            let span_hi = chosen[0];
            let base = ((span_lo - lo as u32) as usize) * per;
            let end = ((span_hi - lo as u32) as usize + 1) * per;
            let mut bound_syms = reply.keysyms[base..end].to_vec();
            let mut map = HashMap::new();
            for (&sym, &kc) in keysyms.iter().zip(chosen.iter()) {
                let off = ((kc - span_lo) as usize) * per;
                bound_syms[off] = sym;
                if per > 1 {
                    bound_syms[off + 1] = sym;
                }
                map.insert(sym, kc);
            }
            let count = (span_hi - span_lo + 1) as u8;
            self.conn
                .change_keyboard_mapping(count, span_lo as u8, per as u8, &bound_syms)
                .map_err(|e| format!("change_keyboard_mapping: {e}"))?
                .check()
                .map_err(|e| format!("change_keyboard_mapping check: {e}"))?;
            // The binding must be live server-side before the first fake press resolves
            // against it.
            self.sync()?;
            seq(&map);
            (span_lo, count, per, bound_syms)
            // Guard drops: ungrab + flush, releasing clients to process the injected
            // events against the still-live bindings.
        };
        // Settle: clients consume the queued MappingNotify + key events and re-fetch the
        // BOUND map before the spares disappear again.
        thread::sleep(TRANSIENT_BIND_SETTLE);
        if let Err(e) = self.restore_transient_binds(span_lo, count, per, &bound_syms, &chosen) {
            eprintln!("[ComputerUse] transient keysym restore failed: {e}");
        }
        Ok(())
    }

    /// Return the transiently bound spares to all-NoSymbol with ONE conditional
    /// `ChangeKeyboardMapping` under its own grab. The span is re-fetched under that
    /// grab and only spares still carrying OUR content are cleared; a keycode another
    /// client (selkies' allocator) claimed during the settle window keeps that client's
    /// content — this restore can never clobber a foreign binding.
    fn restore_transient_binds(
        &self,
        span_lo: u32,
        count: u8,
        per: usize,
        bound_syms: &[u32],
        chosen: &[u32],
    ) -> Result<(), String> {
        self.conn
            .grab_server()
            .map_err(|e| format!("grab_server: {e}"))?;
        let _guard = ServerGrabGuard { conn: &self.conn };
        let reply = self
            .conn
            .get_keyboard_mapping(span_lo as u8, count)
            .map_err(|e| format!("get_keyboard_mapping: {e}"))?
            .reply()
            .map_err(|e| format!("get_keyboard_mapping reply: {e}"))?;
        let cur_per = reply.keysyms_per_keycode as usize;
        if cur_per == 0 || per == 0 {
            return Err("empty keymap".to_string());
        }
        let mut restore = reply.keysyms.clone();
        let mut changed = false;
        for &kc in chosen {
            let sym = bound_syms[((kc - span_lo) as usize) * per];
            let cur = &mut restore[((kc - span_lo) as usize) * cur_per..][..cur_per];
            // Still ours when every populated level carries OUR keysym: the server's XKB
            // integration mirrors a core single-group binding into the group-2 columns,
            // so the refetch shows `sym` at more levels than the bind wrote.
            let still_ours = cur.contains(&sym) && cur.iter().all(|&s| s == 0 || s == sym);
            if still_ours {
                cur.fill(0);
                changed = true;
            }
        }
        if changed {
            self.conn
                .change_keyboard_mapping(count, span_lo as u8, cur_per as u8, &restore)
                .map_err(|e| format!("change_keyboard_mapping: {e}"))?
                .check()
                .map_err(|e| format!("change_keyboard_mapping check: {e}"))?;
        }
        Ok(())
    }
}

/// How long transient binds outlive the injected key events before being restored: the
/// focused client has to wake up, see the bind's MappingNotify, and re-fetch the keymap
/// while the bindings are still live, or the presses translate against the restored map
/// and type nothing.
const TRANSIENT_BIND_SETTLE: Duration = Duration::from_millis(50);

/// RAII server grab release: the grab is dropped (and the request flushed) on every exit
/// path, early error returns and panics included — a leaked server grab freezes every
/// client on the display.
struct ServerGrabGuard<'a> {
    conn: &'a RustConnection,
}

impl Drop for ServerGrabGuard<'_> {
    fn drop(&mut self) {
        let _ = self.conn.ungrab_server().map(|c| c.ignore_error());
        let _ = self.conn.flush();
    }
}

impl Drop for CuX11Backend {
    /// Closing the connection can race the server's client teardown against still-buffered
    /// fake-input requests (observed as lost button releases); one round trip forces the
    /// server to consume everything sent on this connection before it goes away.
    fn drop(&mut self) {
        if let Ok(cookie) = self.conn.get_input_focus() {
            let _ = cookie.reply();
        }
    }
}

impl CuBackend for CuX11Backend {
    fn name(&self) -> &'static str {
        "x11"
    }

    fn fb_size(&self) -> Result<(i32, i32), String> {
        let (w, h) = self.root_geometry()?;
        Ok((w as i32, h as i32))
    }

    fn key(&self, scancode: u32, pressed: bool) {
        if scancode > u8::MAX as u32 {
            return;
        }
        let kind = if pressed {
            KEY_PRESS_EVENT
        } else {
            KEY_RELEASE_EVENT
        };
        self.fake_input(kind, scancode as u8, x11rb::NONE, 0, 0);
    }

    fn mouse_move(&self, x: f64, y: f64) {
        // detail = 0 makes the motion absolute in root coordinates.
        self.fake_input(
            MOTION_NOTIFY_EVENT,
            0,
            self.root,
            x.round() as i16,
            y.round() as i16,
        );
    }

    fn button(&self, btn: CuButton, pressed: bool) {
        let detail = match btn {
            CuButton::Left => 1,
            CuButton::Middle => 2,
            CuButton::Right => 3,
        };
        let kind = if pressed {
            BUTTON_PRESS_EVENT
        } else {
            BUTTON_RELEASE_EVENT
        };
        self.fake_input(kind, detail, x11rb::NONE, 0, 0);
    }

    fn scroll(&self, dx: f64, dy: f64) {
        // X has no smooth axis over XTEST: a scroll is N discrete clicks of the wheel
        // buttons (4 = up, 5 = down, 6 = left, 7 = right), one press/release pair each.
        let emit = |button: u8, clicks: i32| {
            for _ in 0..clicks.min(MAX_SCROLL_CLICKS) {
                self.fake_input(BUTTON_PRESS_EVENT, button, x11rb::NONE, 0, 0);
                self.fake_input(BUTTON_RELEASE_EVENT, button, x11rb::NONE, 0, 0);
            }
        };
        let vy = dy.round() as i32;
        let vx = dx.round() as i32;
        if vy != 0 {
            emit(if vy < 0 { 4 } else { 5 }, vy.abs());
        }
        if vx != 0 {
            emit(if vx < 0 { 6 } else { 7 }, vx.abs());
        }
    }

    fn screenshot_png(&self, display: u32) -> Result<Vec<u8>, String> {
        // One X server, one root: only display 0 exists on this backend.
        if display != 0 {
            return Err(format!("Unknown display: {display}"));
        }
        let (w, h) = self.root_geometry()?;
        let img = self
            .conn
            .get_image(ImageFormat::Z_PIXMAP, self.root, 0, 0, w, h, !0u32)
            .map_err(|e| format!("get_image: {e}"))?
            .reply()
            .map_err(|e| format!("get_image reply: {e}"))?;
        let mut data = img.data;
        let expected = w as usize * h as usize * 4;
        if data.len() != expected {
            return Err(format!(
                "unexpected image size {} for {}x{} (only 32-bpp roots are supported)",
                data.len(),
                w,
                h
            ));
        }
        // The agent needs to see the pointer; the stream's cursor settings do not apply here.
        if self.has_xfixes
            && let Some(c) = self
                .conn
                .xfixes_get_cursor_image()
                .ok()
                .and_then(|c| c.reply().ok())
            && c.width > 0
            && c.height > 0
        {
            let (img_x, img_y) = super::cursor_image_origin(c.x, c.y, c.xhot, c.yhot, 0, 0);
            super::overlay_cursor(
                &mut data,
                w as usize * 4,
                w as i32,
                h as i32,
                c.width as i32,
                c.height as i32,
                &c.cursor_image,
                img_x,
                img_y,
            );
        }
        // The grab is BGRX; the padding byte is undefined for depth-24 roots, so alpha is
        // forced opaque or the PNG would come out transparent.
        for px in data.as_chunks_mut::<4>().0 {
            px.swap(0, 2);
            px[3] = 0xFF;
        }
        encode_png_rgba(&data, w as u32, h as u32)
    }

    fn cursor_pos(&self) -> Result<(f64, f64), String> {
        let ptr = self
            .conn
            .query_pointer(self.root)
            .map_err(|e| format!("query_pointer: {e}"))?
            .reply()
            .map_err(|e| format!("query_pointer reply: {e}"))?;
        Ok((ptr.root_x as f64, ptr.root_y as f64))
    }

    fn resolve_keysyms(&self, keysyms: &[u32]) -> Vec<(u32, u32)> {
        let mut cached = self.reverse_keymap.borrow_mut();
        let km = cached.get_or_insert_with(|| self.build_reverse_keymap());
        keysyms
            .iter()
            .map(|sym| km.by_sym.get(sym).copied().unwrap_or((0, 0)))
            .collect()
    }

    fn altgr_keycode(&self) -> u32 {
        let mut cached = self.reverse_keymap.borrow_mut();
        cached
            .get_or_insert_with(|| self.build_reverse_keymap())
            .altgr_keycode
    }

    fn with_transient_keysyms(&self, keysyms: &[u32], seq: &mut dyn FnMut(&HashMap<u32, u32>)) {
        // Dedup defensively; a duplicate would burn a spare keycode for nothing.
        let mut unique: Vec<u32> = Vec::with_capacity(keysyms.len());
        for &s in keysyms {
            if s != 0 && !unique.contains(&s) {
                unique.push(s);
            }
        }
        if unique.is_empty() {
            seq(&HashMap::new());
            return;
        }
        if let Err(e) = self.type_with_transient_binds(&unique, seq) {
            eprintln!("[ComputerUse] transient keysym bind failed ({e}); typing without it");
            seq(&HashMap::new());
        }
    }
}
