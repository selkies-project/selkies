/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The compositor keymap owner's names where no compositor runs: the text spares the X11
//! keyboard shim reads, and an owner that never builds -- the answer a Linux session gives
//! a seat whose keymap cannot be read -- so the Wayland keyboard ladder falls through to
//! its next rung. The Windows injection milestone (SendInput) lands in the place this file
//! holds.

use std::sync::Arc;

use super::seat::WaylandSeat;

/// Spare keycodes every client types a character on: F13 to F24, then the JIS IntlRo and
/// IntlYen keys. A Wayland client may read a key's code as the physical key it names before the
/// keysym the keymap gives, and Chromium drops a code naming no key it knows and runs its
/// browser, media, and launcher keys as commands, so a character on another spare reaches every
/// client but Chromium.
pub const TEXT_SPARES: [u32; 14] = [
    191, 192, 193, 194, 195, 196, 197, 198, 199, 200, 201, 202, 97, 132,
];

/// The seat's keysym policy where no compositor runs: `new` reports that no keymap could be
/// compiled, so the ladder that reaches for an owner falls to the next rung, and no key is
/// ever pressed, released, typed, or carried by a rebuild.
pub struct WaylandKeymapOwner;

impl WaylandKeymapOwner {
    /// No compositor runs off Linux to give a keymap.
    pub fn new(_seat: Arc<dyn WaylandSeat>, _base_keymap_text: &str) -> Result<Self, String> {
        Err("no compositor keymap runs on this OS".to_string())
    }

    /// No keymap was built, so no keysym resolves on the base layout; the caller types the
    /// character as text instead.
    pub fn resolves(&self, _keysym: u32) -> bool {
        false
    }

    /// No keymap was built, so no keysym has a base keycode.
    pub fn base_keycode(&self, _keysym: u32) -> Option<u32> {
        None
    }

    /// No keymap was built.
    pub fn map_len(&self) -> usize {
        0
    }

    /// Nothing was pressed through an owner that never built.
    pub fn held_count(&self) -> usize {
        0
    }

    /// No keymap was built, so text is not typed here; the caller falls back per character.
    pub fn type_text(&mut self, _text: &str, _neutralize: bool) -> Result<bool, String> {
        Err("no compositor keymap runs on this OS".to_string())
    }

    /// No keymap was built, so no key is pressed.
    pub fn press(&mut self, _keysym: u32, _neutralize: bool) -> Result<(), String> {
        Err("no compositor keymap runs on this OS".to_string())
    }

    /// No keymap was built, so no key is released.
    pub fn release(&mut self, _keysym: u32) -> Result<(), String> {
        Err("no compositor keymap runs on this OS".to_string())
    }

    /// No keymap was built, so there is nothing held to reset.
    pub fn reset(&mut self) {}

    /// No keymap was built, so nothing is held to carry into a rebuild.
    pub fn adopt_held(&mut self, _previous: &WaylandKeymapOwner) {}
}
