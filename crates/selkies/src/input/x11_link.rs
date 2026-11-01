/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The X server as the input handler drives it: one connection's keymap cache, XTEST, XKB,
//! XFixes, and XInput behind the `XLink` trait, so the keyboard shim and the pointer logic run
//! against a fake keymap in the unit tests and against x11rb in a session.
//!
//! The XKB half carries what the core protocol cannot say (`x11_xkb.py`): the core keymap folds
//! every layout group into one column list, so a keysym's (group, level) cannot be told from it
//! alone. `XkbGetMap` names the group and level every keysym sits at, `XkbGetState` and
//! `XkbLatchLockState` read and lock the group an injection must run under, and
//! `XkbSelectEvents` keeps keymap changes flowing: the server withholds core `MappingNotify`
//! from an XKB-aware client and reports a whole-keyboard replacement (`setxkbmap`) to it only
//! as `XkbNewKeyboardNotify`. Everything XKB is best effort: a server without the extension
//! leaves the link without it and the keyboard falls back to the core keymap.
//!
//! Every method here blocks on the connection; the handler calls them from the X worker
//! thread (`x11_worker`) and never from the runtime.

use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::sync::Mutex;

use log::debug;
use x11rb::connection::Connection;
use x11rb::errors::ReplyError;
use x11rb::protocol::Event;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xinput::{self, ConnectionExt as _};
use x11rb::protocol::xkb::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt as _, Keysym};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

/// Levels 0-3 are what Shift and AltGr reach; deeper levels are not injectable.
pub const XKB_INJECTABLE_LEVELS: usize = 4;

/// Where a keysym sits in the XKB map: keycode, group, and level.
pub type XkbPlacement = (u8, u8, u8);

/// An XFixes cursor image: premultiplied ARGB words, row major.
#[derive(Clone, Debug)]
pub struct XCursorImage {
    pub width: u32,
    pub height: u32,
    pub xhot: i32,
    pub yhot: i32,
    pub serial: u32,
    pub pixels: Vec<u32>,
}

/// A keymap change the server reported on the input connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeymapEvent {
    /// Core `MappingNotify` for the keyboard map over a keycode range.
    Keyboard { first_keycode: u8, count: u8 },
    /// Core `MappingNotify` for the modifier map.
    Modifier,
}

/// The connection to the X server, as the shim and the pointer logic use it.
pub trait XLink: Send + Sync {
    /// The server's keycode range.
    fn keycode_range(&self) -> (u8, u8);
    /// The lowest (column, keycode) carrying `keysym` in the cached core keymap, else 0.
    fn keysym_to_keycode(&self, keysym: u32) -> u8;
    /// The keysym at `level` of `keycode` in the cached core keymap, else 0.
    fn keycode_to_keysym(&self, keycode: u8, level: usize) -> u32;
    /// The core keymap rows for `count` keycodes from `first`, straight from the server.
    fn keyboard_mapping(&self, first: u8, count: u8) -> Result<Vec<Vec<u32>>, String>;
    /// Every keycode the modifier map binds.
    fn modifier_mapping(&self) -> Result<Vec<u8>, String>;
    /// Bind `rows` onto consecutive keycodes from `first` (`ChangeKeyboardMapping`), each row
    /// one keycode's keysyms per column.
    fn change_keyboard_mapping(&self, first: u8, rows: &[Vec<u32>]) -> Result<(), String>;
    /// Refetch the cached core keymap for a keycode range a `MappingNotify` named.
    fn refresh_keyboard_mapping(&self, first: u8, count: u8) -> Result<(), String>;
    /// XTEST key press or release, queued and not flushed.
    fn fake_key(&self, keycode: u8, down: bool);
    /// XTEST pointer warp (absolute) or move (relative), queued and not flushed.
    fn fake_motion(&self, x: i16, y: i16, relative: bool);
    /// XTEST button press or release, queued and not flushed.
    fn fake_button(&self, button: u8, down: bool);
    /// Write out every queued request.
    fn flush(&self);
    /// Round trip to the server, so the requests before it have been processed.
    fn sync(&self) -> Result<(), String>;
    /// Whether the XKEYBOARD link is open.
    fn has_xkb(&self) -> bool;
    /// The XKB symbol map indexed by keysym: the lowest group, then level, then keycode carrying
    /// it at an injectable level.
    fn xkb_placement(&self) -> Result<HashMap<u32, XkbPlacement>, String>;
    /// The group the server has locked for the core keyboard.
    fn xkb_locked_group(&self) -> Result<u8, String>;
    /// Lock the core keyboard's group; queued, not flushed, so the caller's next XTEST key
    /// follows it in the same write.
    fn xkb_lock_group(&self, group: u8);
    /// The root pointer position and the modifier/button mask.
    fn query_pointer(&self) -> Result<(i16, i16, u16), String>;
    /// The current cursor image through XFixes.
    fn cursor_image(&self) -> Result<XCursorImage, String>;
    /// The screen's DPI from its millimeter size, None where the server reports none.
    fn screen_dpi(&self) -> Option<f64>;
    /// The server's autorepeat `(delay ms, interval ms)` from XKB controls.
    fn autorepeat(&self) -> Option<(u16, u16)>;
    /// Whether the server reads kernel input devices itself (`_display_reads_kernel_input`).
    fn reads_kernel_input(&self) -> bool;
    /// Block until the server sends a keymap event; None once the connection is gone, or at
    /// once on a link that delivers no events.
    fn wait_keymap_event(&self) -> Option<KeymapEvent>;
    /// Whether the connection has broken.
    fn broken(&self) -> bool;
    /// Shut the socket down, waking a thread blocked on the connection with an error, so an
    /// abandoned connection ends its threads and gives up its X client slot. Never blocks.
    fn close(&self) {}
}

/// The cached core keymap: every keycode's columns.
struct Keymap {
    min: u8,
    per_keycode: usize,
    syms: Vec<u32>,
}

impl Keymap {
    fn row(&self, keycode: u8) -> &[u32] {
        let i = (keycode as usize).saturating_sub(self.min as usize) * self.per_keycode;
        self.syms.get(i..i + self.per_keycode).unwrap_or(&[])
    }
}

/// An x11rb connection serving `XLink`.
pub struct X11Link {
    conn: RustConnection,
    root: xproto::Window,
    keymap: Mutex<Keymap>,
    xkb: Option<XkbState>,
    xfixes_negotiated: Mutex<bool>,
    broken: Mutex<bool>,
}

/// The XKEYBOARD link's identity on the connection.
struct XkbState {
    core_device: u8,
}

impl X11Link {
    /// Connect to `display` (None for `$DISPLAY`), cache the core keymap, and open the XKB
    /// link where the server offers it.
    pub fn connect(display: Option<&str>) -> Result<Self, String> {
        let (conn, screen) = RustConnection::connect(display).map_err(|e| e.to_string())?;
        let setup = conn.setup();
        let root = setup.roots.get(screen).map(|s| s.root).ok_or("no screen")?;
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let reply = conn
            .get_keyboard_mapping(min, max - min + 1)
            .map_err(ReplyError::from)
            .and_then(|c| c.reply())
            .map_err(|e| e.to_string())?;
        let keymap = Keymap {
            min,
            per_keycode: reply.keysyms_per_keycode as usize,
            syms: reply.keysyms,
        };
        let xkb = Self::open_xkb(&conn);
        Ok(Self {
            conn,
            root,
            keymap: Mutex::new(keymap),
            xkb,
            xfixes_negotiated: Mutex::new(false),
            broken: Mutex::new(false),
        })
    }

    /// Register the connection as XKB-aware and select the keymap events that registration
    /// would otherwise silence: `XkbNewKeyboardNotify` for keyboard replacements, and
    /// `XkbMapNotify`, whose selection is what keeps the server sending core `MappingNotify`
    /// for partial remaps to this connection.
    fn open_xkb(conn: &RustConnection) -> Option<XkbState> {
        let hello = conn.xkb_use_extension(1, 0).ok()?.reply().ok()?;
        if !hello.supported {
            return None;
        }
        let state = conn
            .xkb_get_state(xkb::ID::USE_CORE_KBD.into())
            .ok()?
            .reply()
            .ok()?;
        // MapNotify's details are `affect_map`/`map`, not the switch, and x11rb derives the
        // request's `affectWhich` from `select_all` and the switch.
        let aux = xkb::SelectEventsAux::new();
        let mask = xkb::EventType::NEW_KEYBOARD_NOTIFY | xkb::EventType::MAP_NOTIFY;
        if let Err(e) = conn.xkb_select_events(
            xkb::ID::USE_CORE_KBD.into(),
            xkb::EventType::from(0u16),
            mask,
            xkb::MapPart::from(0x00FFu16),
            xkb::MapPart::from(0x00FFu16),
            &aux,
        ) {
            debug!(target: "input", "XKB event selection failed ({e}); core keymap only");
            return None;
        }
        let _ = conn.flush();
        Some(XkbState {
            core_device: state.device_id,
        })
    }

    fn mark_broken(&self) {
        if let Ok(mut b) = self.broken.lock() {
            *b = true;
        }
    }

    /// A request's result as text, marking the connection broken on a connection error (an X
    /// error reply leaves the connection usable).
    fn check<T>(&self, result: Result<T, impl Into<ReplyError>>) -> Result<T, String> {
        result.map_err(|e| {
            let e: ReplyError = e.into();
            if matches!(e, ReplyError::ConnectionError(_)) {
                self.mark_broken();
            }
            e.to_string()
        })
    }

    /// The keymap change an `XkbNewKeyboardNotify` or `MappingNotify` reports, as the shim reads it.
    fn keymap_event(&self, event: Event) -> Option<KeymapEvent> {
        match event {
            Event::MappingNotify(e) => match e.request {
                xproto::Mapping::KEYBOARD => Some(KeymapEvent::Keyboard {
                    first_keycode: e.first_keycode,
                    count: e.count,
                }),
                xproto::Mapping::MODIFIER => Some(KeymapEvent::Modifier),
                _ => None,
            },
            Event::XkbNewKeyboardNotify(e) => {
                // The server emits one per device; only the core keyboard's counts, so a
                // layout switch is handled once.
                let core = self.xkb.as_ref().map(|x| x.core_device);
                if core != Some(e.device_id) {
                    return None;
                }
                Some(KeymapEvent::Keyboard {
                    first_keycode: e.min_key_code,
                    count: e.max_key_code.wrapping_sub(e.min_key_code).wrapping_add(1),
                })
            }
            _ => None,
        }
    }
}

impl XLink for X11Link {
    fn keycode_range(&self) -> (u8, u8) {
        let s = self.conn.setup();
        (s.min_keycode, s.max_keycode)
    }

    fn keysym_to_keycode(&self, keysym: u32) -> u8 {
        let Ok(km) = self.keymap.lock() else { return 0 };
        let (min, max) = self.keycode_range();
        let mut best: Option<(usize, u8)> = None;
        for kc in min..=max {
            for (col, sym) in km.row(kc).iter().enumerate() {
                if *sym == keysym && best.is_none_or(|(c, k)| (col, kc) < (c, k)) {
                    best = Some((col, kc));
                }
            }
        }
        best.map(|(_, kc)| kc).unwrap_or(0)
    }

    fn keycode_to_keysym(&self, keycode: u8, level: usize) -> u32 {
        self.keymap
            .lock()
            .ok()
            .and_then(|km| km.row(keycode).get(level).copied())
            .unwrap_or(0)
    }

    fn keyboard_mapping(&self, first: u8, count: u8) -> Result<Vec<Vec<u32>>, String> {
        let reply = self.check(
            self.conn
                .get_keyboard_mapping(first, count)
                .map_err(ReplyError::from)
                .and_then(|c| c.reply()),
        )?;
        let per = reply.keysyms_per_keycode as usize;
        Ok(reply
            .keysyms
            .chunks(per.max(1))
            .map(|r| r.to_vec())
            .collect())
    }

    fn modifier_mapping(&self) -> Result<Vec<u8>, String> {
        let reply = self.check(
            self.conn
                .get_modifier_mapping()
                .map_err(ReplyError::from)
                .and_then(|c| c.reply()),
        )?;
        Ok(reply.keycodes.into_iter().filter(|kc| *kc != 0).collect())
    }

    fn change_keyboard_mapping(&self, first: u8, rows: &[Vec<u32>]) -> Result<(), String> {
        let per = rows.iter().map(Vec::len).max().unwrap_or(0).max(1);
        let mut syms: Vec<Keysym> = Vec::with_capacity(rows.len() * per);
        for row in rows {
            syms.extend(row.iter().copied());
            syms.extend(std::iter::repeat_n(0, per - row.len()));
        }
        let count = u8::try_from(rows.len()).map_err(|_| "too many keycodes")?;
        let per = u8::try_from(per).map_err(|_| "too many columns")?;
        self.check(
            self.conn
                .change_keyboard_mapping(count, first, per, &syms)
                .map(|_| ()),
        )?;
        if let Ok(mut km) = self.keymap.lock() {
            // Mirror the bind into the cache: the server's notify refreshes it too, but a press
            // follows the bind before that notify is read.
            let width = km.per_keycode;
            for (i, row) in rows.iter().enumerate() {
                let kc = first as usize + i;
                let start = kc.saturating_sub(km.min as usize) * width;
                if start + width <= km.syms.len() {
                    for (col, slot) in km.syms[start..start + width].iter_mut().enumerate() {
                        *slot = row.get(col).copied().unwrap_or(0);
                    }
                }
            }
        }
        Ok(())
    }

    fn refresh_keyboard_mapping(&self, first: u8, count: u8) -> Result<(), String> {
        let reply = self.check(
            self.conn
                .get_keyboard_mapping(first, count)
                .map_err(ReplyError::from)
                .and_then(|c| c.reply()),
        )?;
        let mut km = self.keymap.lock().map_err(|_| "keymap cache poisoned")?;
        if reply.keysyms_per_keycode as usize != km.per_keycode {
            // The width changed: refetch the whole map rather than splice mismatched rows.
            let (min, max) = self.keycode_range();
            let all = self.check(
                self.conn
                    .get_keyboard_mapping(min, max - min + 1)
                    .map_err(ReplyError::from)
                    .and_then(|c| c.reply()),
            )?;
            *km = Keymap {
                min,
                per_keycode: all.keysyms_per_keycode as usize,
                syms: all.keysyms,
            };
            return Ok(());
        }
        let width = km.per_keycode;
        let start = (first as usize).saturating_sub(km.min as usize) * width;
        if start >= km.syms.len() {
            return Ok(());
        }
        let end = (start + reply.keysyms.len()).min(km.syms.len());
        km.syms[start..end].copy_from_slice(&reply.keysyms[..end - start]);
        Ok(())
    }

    fn fake_key(&self, keycode: u8, down: bool) {
        let kind = if down {
            xproto::KEY_PRESS_EVENT
        } else {
            xproto::KEY_RELEASE_EVENT
        };
        if self
            .conn
            .xtest_fake_input(kind, keycode, x11rb::CURRENT_TIME, x11rb::NONE, 0, 0, 0)
            .is_err()
        {
            self.mark_broken();
        }
    }

    fn fake_motion(&self, x: i16, y: i16, relative: bool) {
        if self
            .conn
            .xtest_fake_input(
                xproto::MOTION_NOTIFY_EVENT,
                relative as u8,
                x11rb::CURRENT_TIME,
                x11rb::NONE,
                x,
                y,
                0,
            )
            .is_err()
        {
            self.mark_broken();
        }
    }

    fn fake_button(&self, button: u8, down: bool) {
        let kind = if down {
            xproto::BUTTON_PRESS_EVENT
        } else {
            xproto::BUTTON_RELEASE_EVENT
        };
        if self
            .conn
            .xtest_fake_input(kind, button, x11rb::CURRENT_TIME, x11rb::NONE, 0, 0, 0)
            .is_err()
        {
            self.mark_broken();
        }
    }

    fn flush(&self) {
        if self.conn.flush().is_err() {
            self.mark_broken();
        }
    }

    fn sync(&self) -> Result<(), String> {
        self.check(
            self.conn
                .get_input_focus()
                .map_err(ReplyError::from)
                .and_then(|c| c.reply()),
        )
        .map(|_| ())
    }

    fn has_xkb(&self) -> bool {
        self.xkb.is_some()
    }

    fn xkb_placement(&self) -> Result<HashMap<u32, XkbPlacement>, String> {
        if self.xkb.is_none() {
            return Err("no XKB link".into());
        }
        let (lo, hi) = self.keycode_range();
        let reply = self.check(
            self.conn
                .xkb_get_map(
                    xkb::ID::USE_CORE_KBD.into(),
                    xkb::MapPart::from(0u16),
                    xkb::MapPart::KEY_SYMS,
                    0,
                    0,
                    lo,
                    hi - lo + 1,
                    0,
                    0,
                    0,
                    0,
                    xkb::VMod::from(0u16),
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                )
                .map_err(ReplyError::from)
                .and_then(|c| c.reply()),
        )?;
        let mut placement: HashMap<u32, XkbPlacement> = HashMap::new();
        let Some(syms) = reply.map.syms_rtrn else {
            return Ok(placement);
        };
        for (i, key) in syms.iter().enumerate() {
            let keycode = reply.first_key_sym.wrapping_add(i as u8);
            let ngroups = (key.group_info & 0x0F) as usize;
            let width = key.width as usize;
            for group in 0..ngroups {
                for level in 0..width.min(XKB_INJECTABLE_LEVELS) {
                    let sym = key.syms.get(group * width + level).copied().unwrap_or(0);
                    if sym == 0 {
                        continue;
                    }
                    let candidate = (keycode, group as u8, level as u8);
                    let better = match placement.get(&sym) {
                        None => true,
                        Some(have) => {
                            (candidate.1, candidate.2, candidate.0) < (have.1, have.2, have.0)
                        }
                    };
                    if better {
                        placement.insert(sym, candidate);
                    }
                }
            }
        }
        Ok(placement)
    }

    fn xkb_locked_group(&self) -> Result<u8, String> {
        if self.xkb.is_none() {
            return Err("no XKB link".into());
        }
        let reply = self.check(
            self.conn
                .xkb_get_state(xkb::ID::USE_CORE_KBD.into())
                .map_err(ReplyError::from)
                .and_then(|c| c.reply()),
        )?;
        Ok(u8::from(reply.locked_group))
    }

    fn xkb_lock_group(&self, group: u8) {
        if self.xkb.is_none() {
            return;
        }
        let r = self.conn.xkb_latch_lock_state(
            xkb::ID::USE_CORE_KBD.into(),
            xproto::ModMask::from(0u16),
            xproto::ModMask::from(0u16),
            true,
            xkb::Group::from(group),
            xproto::ModMask::from(0u16),
            false,
            0,
        );
        match r {
            Ok(cookie) => cookie.ignore_error(),
            Err(_) => self.mark_broken(),
        }
    }

    fn query_pointer(&self) -> Result<(i16, i16, u16), String> {
        let reply = self.check(
            self.conn
                .query_pointer(self.root)
                .map_err(ReplyError::from)
                .and_then(|c| c.reply()),
        )?;
        Ok((reply.root_x, reply.root_y, u16::from(reply.mask)))
    }

    fn cursor_image(&self) -> Result<XCursorImage, String> {
        // XFixes wants version negotiation before any other request, and this fetch runs
        // whether or not a capture is up.
        let negotiated = self.xfixes_negotiated.lock().map(|n| *n).unwrap_or(false);
        if !negotiated {
            self.check(
                self.conn
                    .xfixes_query_version(6, 0)
                    .map_err(ReplyError::from)
                    .and_then(|c| c.reply()),
            )?;
            if let Ok(mut n) = self.xfixes_negotiated.lock() {
                *n = true;
            }
        }
        let reply = self.check(
            self.conn
                .xfixes_get_cursor_image()
                .map_err(ReplyError::from)
                .and_then(|c| c.reply()),
        )?;
        Ok(XCursorImage {
            width: reply.width as u32,
            height: reply.height as u32,
            xhot: reply.xhot as i32,
            yhot: reply.yhot as i32,
            serial: reply.cursor_serial,
            pixels: reply.cursor_image,
        })
    }

    fn screen_dpi(&self) -> Option<f64> {
        let screen = self
            .conn
            .setup()
            .roots
            .iter()
            .find(|s| s.root == self.root)?;
        let (w_mm, h_mm) = (
            screen.width_in_millimeters as f64,
            screen.height_in_millimeters as f64,
        );
        if w_mm <= 0.0 || h_mm <= 0.0 {
            return None;
        }
        let dpi_x = screen.width_in_pixels as f64 * 25.4 / w_mm;
        let dpi_y = screen.height_in_pixels as f64 * 25.4 / h_mm;
        Some((dpi_x + dpi_y) / 2.0)
    }

    fn autorepeat(&self) -> Option<(u16, u16)> {
        self.xkb.as_ref()?;
        let reply = self
            .conn
            .xkb_get_controls(xkb::ID::USE_CORE_KBD.into())
            .ok()?
            .reply()
            .ok()?;
        Some((reply.repeat_delay, reply.repeat_interval))
    }

    fn reads_kernel_input(&self) -> bool {
        let Ok(Ok(atom)) = self
            .conn
            .intern_atom(true, b"Device Node")
            .map(|c| c.reply())
        else {
            return false;
        };
        if atom.atom == x11rb::NONE {
            return false;
        }
        let Ok(Ok(devices)) = self
            .conn
            .xinput_xi_query_device(xinput::Device::ALL)
            .map(|c| c.reply())
        else {
            debug!(target: "input", "XInput device query failed");
            return false;
        };
        devices.infos.iter().any(|dev| {
            self.conn
                .xinput_xi_list_properties(dev.deviceid)
                .ok()
                .and_then(|c| c.reply().ok())
                .is_some_and(|props| props.properties.contains(&atom.atom))
        })
    }

    fn wait_keymap_event(&self) -> Option<KeymapEvent> {
        loop {
            match self.conn.wait_for_event() {
                Ok(event) => {
                    if let Some(e) = self.keymap_event(event) {
                        return Some(e);
                    }
                }
                Err(_) => {
                    self.mark_broken();
                    return None;
                }
            }
        }
    }

    fn broken(&self) -> bool {
        self.broken.lock().map(|b| *b).unwrap_or(true)
    }

    fn close(&self) {
        // An ENOTCONN from a socket already down is the state this asks for.
        let _ = nix::sys::socket::shutdown(
            self.conn.stream().as_raw_fd(),
            nix::sys::socket::Shutdown::Both,
        );
    }
}

/// The X server's pointer, asked on a connection of its own (`_XPointerReader`), so a display
/// server slow to answer holds the worker thread it is read from and never the runtime. The
/// connection opens on first use and again after a failure.
#[derive(Default)]
pub struct XPointerReader {
    conn: Mutex<Option<(RustConnection, xproto::Window)>>,
    display: Option<String>,
}

impl XPointerReader {
    /// A reader for `display` (None for `$DISPLAY`).
    pub fn new(display: Option<String>) -> Self {
        Self {
            conn: Mutex::new(None),
            display,
        }
    }

    /// Blocking. The root position, or None when the server cannot be asked.
    pub fn read(&self) -> Option<(i16, i16)> {
        let mut guard = self.conn.lock().ok()?;
        if guard.is_none() {
            match RustConnection::connect(self.display.as_deref()) {
                Ok((conn, screen)) => {
                    let root = conn.setup().roots.get(screen)?.root;
                    *guard = Some((conn, root));
                }
                Err(e) => {
                    debug!(target: "input", "pointer position read failed: {e}");
                    return None;
                }
            }
        }
        let (conn, root) = guard.as_ref()?;
        match conn
            .query_pointer(*root)
            .map_err(ReplyError::from)
            .and_then(|c| c.reply())
        {
            Ok(reply) => Some((reply.root_x, reply.root_y)),
            Err(e) => {
                debug!(target: "input", "pointer position read failed: {e}");
                *guard = None;
                None
            }
        }
    }

    /// Drop the connection.
    pub fn close(&self) {
        if let Ok(mut guard) = self.conn.lock() {
            *guard = None;
        }
    }
}

/// The Unix socket path of a local X display name (`:N`, `:N.S`, `unix:N`); None for a TCP or
/// remote name.
pub fn x_display_socket(name: &str) -> Option<String> {
    let rest = name.trim().strip_prefix("unix").unwrap_or(name.trim());
    let rest = rest.strip_prefix(':')?;
    let number: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if number.is_empty() {
        return None;
    }
    let tail = &rest[number.len()..];
    if !tail.is_empty() {
        let screen = tail.strip_prefix('.')?;
        if screen.is_empty() || !screen.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
    }
    Some(format!("/tmp/.X11-unix/X{number}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_sockets() {
        assert_eq!(
            x_display_socket(":99").as_deref(),
            Some("/tmp/.X11-unix/X99")
        );
        assert_eq!(
            x_display_socket("unix:1.0").as_deref(),
            Some("/tmp/.X11-unix/X1")
        );
        assert_eq!(x_display_socket("host:1"), None);
        assert_eq!(x_display_socket(":1.x"), None);
    }

    /// Needs the sandbox's Xvfb on `DISPLAY`.
    #[test]
    #[ignore = "needs an X display (DISPLAY=:99 in the sandbox)"]
    fn connects_and_reads_the_keymap() {
        let link = X11Link::connect(None).expect("connect");
        let (lo, hi) = link.keycode_range();
        assert!(lo >= 8 && hi > lo);
        assert!(
            link.keysym_to_keycode(0x61) != 0 || !link.has_xkb(),
            "a keymap carries 'a'"
        );
        let dpi = link.screen_dpi();
        assert!(dpi.is_none_or(|d| d > 10.0));
        if link.has_xkb() {
            let placed = link.xkb_placement().expect("xkb map");
            assert!(placed.contains_key(&0x61));
            assert!(link.xkb_locked_group().is_ok());
            let (delay, interval) = link.autorepeat().expect("XKB controls");
            assert!(
                delay > 0 && interval > 0,
                "the server's autorepeat: {delay} ms, {interval} ms"
            );
        }
        assert!(link.query_pointer().is_ok());
    }
}
