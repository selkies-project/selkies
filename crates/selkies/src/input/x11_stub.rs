/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The X server's names where no X server runs: the `XLink` contract the keyboard shim and
//! the X thread are written against, the connection and the pointer reader that never open,
//! and the cursor, keymap, and placement types the cursor encoder names -- so the handler
//! and the dispatcher keep their shape and every X reach answers the absence the way a Linux
//! build answers a display that is not there: a connection that does not come up, a
//! keymap with no keys, a position never read. The Windows injection milestone (SendInput)
//! lands in the place this file holds.

use std::collections::HashMap;

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

/// The X server's connection where none runs: `connect` reports the absence, the answer a
/// Linux build reads as a display that is not there, so the handler's ladder stays on its
/// lower rungs and keeps re-probing.
pub struct X11Link;

impl X11Link {
    /// No X server runs off Linux.
    pub fn connect(_display: Option<&str>) -> Result<Self, String> {
        Err("no X server runs on this OS".to_string())
    }
}

impl XLink for X11Link {
    fn keycode_range(&self) -> (u8, u8) {
        (0, 0)
    }

    fn keysym_to_keycode(&self, _keysym: u32) -> u8 {
        0
    }

    fn keycode_to_keysym(&self, _keycode: u8, _level: usize) -> u32 {
        0
    }

    fn keyboard_mapping(&self, _first: u8, _count: u8) -> Result<Vec<Vec<u32>>, String> {
        Err("no X server runs on this OS".to_string())
    }

    fn modifier_mapping(&self) -> Result<Vec<u8>, String> {
        Err("no X server runs on this OS".to_string())
    }

    fn change_keyboard_mapping(&self, _first: u8, _rows: &[Vec<u32>]) -> Result<(), String> {
        Err("no X server runs on this OS".to_string())
    }

    fn refresh_keyboard_mapping(&self, _first: u8, _count: u8) -> Result<(), String> {
        Err("no X server runs on this OS".to_string())
    }

    fn fake_key(&self, _keycode: u8, _down: bool) {}

    fn fake_motion(&self, _x: i16, _y: i16, _relative: bool) {}

    fn fake_button(&self, _button: u8, _down: bool) {}

    fn flush(&self) {}

    fn sync(&self) -> Result<(), String> {
        Err("no X server runs on this OS".to_string())
    }

    fn has_xkb(&self) -> bool {
        false
    }

    fn xkb_placement(&self) -> Result<HashMap<u32, XkbPlacement>, String> {
        Err("no X server runs on this OS".to_string())
    }

    fn xkb_locked_group(&self) -> Result<u8, String> {
        Err("no X server runs on this OS".to_string())
    }

    fn xkb_lock_group(&self, _group: u8) {}

    fn query_pointer(&self) -> Result<(i16, i16, u16), String> {
        Err("no X server runs on this OS".to_string())
    }

    fn cursor_image(&self) -> Result<XCursorImage, String> {
        Err("no X server runs on this OS".to_string())
    }

    fn screen_dpi(&self) -> Option<f64> {
        None
    }

    fn autorepeat(&self) -> Option<(u16, u16)> {
        None
    }

    fn reads_kernel_input(&self) -> bool {
        false
    }

    fn wait_keymap_event(&self) -> Option<KeymapEvent> {
        None
    }

    fn broken(&self) -> bool {
        true
    }
}

/// The X server's pointer where none runs: no position is ever read, the answer the reader
/// of a display that is not there gives, so a trackpad page's echo answers `pointer,none`.
pub struct XPointerReader;

impl XPointerReader {
    /// A reader for `display` (None for `$DISPLAY`); no display is ever asked.
    pub fn new(_display: Option<String>) -> Self {
        XPointerReader
    }

    /// No display answers, so no position is read.
    pub fn read(&self) -> Option<(i16, i16)> {
        None
    }

    /// Nothing was opened, so there is nothing to drop.
    pub fn close(&self) {}
}
