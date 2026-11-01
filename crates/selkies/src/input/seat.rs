/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The Wayland compositor seat as the input handler drives it: the pixelflux surface behind
//! one trait, so the handler's policy runs against a recording stand-in in the unit tests and
//! against `pixelflux::ScreenCapture` in a session.
//!
//! Every method maps one to one onto a pixelflux call; nothing here decides policy. A call
//! that pixelflux answers from its compositor thread (`get_xkb_keymap_string`,
//! `get_keyboard_state`, `set_xkb_layout`) blocks for the round trip, and a typer call
//! (`type_keysyms_wayland`) runs a one-shot Wayland client to completion, so the handler runs
//! those on a blocking thread; `inject_*` and the keymap swaps are channel sends.

use pixelflux::{Error, ScreenCapture};

/// What the handler needs from the compositor seat, in pixelflux's own terms.
pub trait WaylandSeat: Send + Sync {
    /// Press (`state` 1) or release (0) an XKB keycode on the seat.
    fn inject_key(&self, keycode: u32, state: u32) -> Result<(), Error>;
    /// Inject an ordered run of `(keycode, state)` events in one message.
    fn inject_keys(&self, events: Vec<(u32, u32)>) -> Result<(), Error>;
    /// Bind `(keycode, keysym)` pairs onto the current base keymap in one swap, ordered with
    /// the key events on the one compositor channel.
    fn set_keymap_overlay(&self, binds: Vec<(u32, u32)>) -> Result<(), Error>;
    /// The seat's base keymap as XKB text.
    fn get_xkb_keymap_string(&self) -> Result<String, Error>;
    /// Set the seat's base layout from RMLVO names; false when the layout failed to compile.
    fn set_xkb_layout(
        &self,
        layout: String,
        variant: String,
        options: String,
        model: String,
        rules: String,
    ) -> Result<bool, Error>;
    /// `(pressed keycodes, modifier mask)` with mask bit 16 Caps Lock.
    fn get_keyboard_state(&self) -> Result<(Vec<u32>, u32), Error>;
    /// Tap keysyms through `display`'s virtual keyboard; `Error::Unavailable` when the
    /// compositor lacks the protocol.
    fn type_keysyms_wayland(&self, display: String, keysyms: Vec<u32>) -> Result<(), Error>;
    /// Warp the pointer; the move's number, None without a running compositor.
    fn inject_mouse_move(&self, x: f64, y: f64) -> Result<Option<u64>, Error>;
    /// Move the pointer by a delta; the move's number, None without a running compositor.
    fn inject_relative_mouse_move(&self, dx: f64, dy: f64) -> Result<Option<u64>, Error>;
    /// Press or release an evdev button code.
    fn inject_mouse_button(&self, button: u32, state: u32) -> Result<(), Error>;
    /// A wheel's axis values.
    fn inject_mouse_scroll(&self, x: f64, y: f64) -> Result<(), Error>;
    /// A touchpad finger's scroll travel.
    fn inject_finger_scroll(&self, dx: f64, dy: f64) -> Result<(), Error>;
    /// The finger's lift.
    fn inject_finger_scroll_end(&self) -> Result<(), Error>;
    /// Where the pointer is, as `(x, y, scale, motion)`; None where the session cannot say.
    fn pointer_location(&self) -> Option<(f64, f64, f64, u64)>;
    /// The compositor the apps run under, for pixelflux's own injection paths.
    fn set_app_wayland_display(&self, display: String);
    /// Stop watching the app compositor's selection.
    fn clipboard_unwatch_app(&self, display: String);
}

impl WaylandSeat for ScreenCapture {
    fn inject_key(&self, keycode: u32, state: u32) -> Result<(), Error> {
        ScreenCapture::inject_key(self, keycode, state)
    }

    fn inject_keys(&self, events: Vec<(u32, u32)>) -> Result<(), Error> {
        ScreenCapture::inject_keys(self, events)
    }

    fn set_keymap_overlay(&self, binds: Vec<(u32, u32)>) -> Result<(), Error> {
        ScreenCapture::set_keymap_overlay(self, binds)
    }

    fn get_xkb_keymap_string(&self) -> Result<String, Error> {
        ScreenCapture::get_xkb_keymap_string(self)
    }

    fn set_xkb_layout(
        &self,
        layout: String,
        variant: String,
        options: String,
        model: String,
        rules: String,
    ) -> Result<bool, Error> {
        ScreenCapture::set_xkb_layout(self, layout, variant, options, model, rules)
    }

    fn get_keyboard_state(&self) -> Result<(Vec<u32>, u32), Error> {
        ScreenCapture::get_keyboard_state(self)
    }

    fn type_keysyms_wayland(&self, display: String, keysyms: Vec<u32>) -> Result<(), Error> {
        ScreenCapture::type_keysyms_wayland(self, display, keysyms)
    }

    fn inject_mouse_move(&self, x: f64, y: f64) -> Result<Option<u64>, Error> {
        ScreenCapture::inject_mouse_move(self, x, y)
    }

    fn inject_relative_mouse_move(&self, dx: f64, dy: f64) -> Result<Option<u64>, Error> {
        ScreenCapture::inject_relative_mouse_move(self, dx, dy)
    }

    fn inject_mouse_button(&self, button: u32, state: u32) -> Result<(), Error> {
        ScreenCapture::inject_mouse_button(self, button, state)
    }

    fn inject_mouse_scroll(&self, x: f64, y: f64) -> Result<(), Error> {
        ScreenCapture::inject_mouse_scroll(self, x, y)
    }

    fn inject_finger_scroll(&self, dx: f64, dy: f64) -> Result<(), Error> {
        ScreenCapture::inject_finger_scroll(self, dx, dy)
    }

    fn inject_finger_scroll_end(&self) -> Result<(), Error> {
        ScreenCapture::inject_finger_scroll_end(self)
    }

    fn pointer_location(&self) -> Option<(f64, f64, f64, u64)> {
        ScreenCapture::pointer_location(self)
    }

    fn set_app_wayland_display(&self, display: String) {
        ScreenCapture::set_app_wayland_display(self, display)
    }

    fn clipboard_unwatch_app(&self, display: String) {
        ScreenCapture::clipboard_unwatch_app(self, display)
    }
}
