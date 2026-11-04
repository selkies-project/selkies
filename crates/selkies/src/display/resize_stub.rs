/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The resize and layout names where no X server runs: every entry point the transports,
//! the session wiring, and the resize tool reach, answering that no RandR runs on this OS
//! the way a Linux build answers a server that refuses -- a resize that cannot be made, a
//! framebuffer that cannot be grown, a layout the caller falls back from, a root that
//! cannot be read -- so the transports keep their shape and every refusal stays the one
//! line its caller already logs. The capture itself is pixelflux's and starts regardless
//! of any of this; the Windows display milestone lands in the place this file holds.

use super::Layouts;

/// Current and fitted resolution information for the first connected output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolutionInfo {
    /// No root exists off Linux to size, so `current` names nothing; the callers that
    /// parse it fall back to the resolution the session seeds.
    pub current: String,
    /// The request echoed back: no output runs off Linux to fit it.
    pub fitted: String,
    /// No output runs off Linux to list modes for.
    pub modes: Vec<String>,
    /// The ceiling as `WxH`; the request echoed back where nothing names a ceiling.
    pub max: String,
    /// No RandR output runs off Linux.
    pub output: Option<String>,
}

/// Current/fitted resolution info: no RandR query runs off Linux, so the answer names no
/// current size and no output, and a caller sizing a capture keeps the resolution it
/// seeded.
pub async fn get_new_res(res_str: &str) -> ResolutionInfo {
    ResolutionInfo {
        current: String::new(),
        fitted: res_str.to_string(),
        modes: Vec::new(),
        max: res_str.to_string(),
        output: None,
    }
}

/// Ensure a `res_str` mode is attached to the connected output; no RandR output runs off
/// Linux to attach one to, so the caller falls to its subprocess chain as it does where
/// the native call fails.
pub async fn ensure_mode(_res_str: &str, _refresh: Option<f64>) -> bool {
    false
}

/// Keep every connected output at the stream's refresh; no RandR output runs off Linux,
/// and the refresh is the desktop's own.
pub async fn refresh_output_mode(_refresh: Option<f64>) {}

/// Resize the display to `res_str`: no RandR runs off Linux, so no resize is made and the
/// caller keeps the size it has, the answer a Linux build gives a server that refused every
/// mode.
pub async fn resize_display(
    _res_str: &str,
    _refresh: Option<f64>,
    _output_size: Option<(i32, i32)>,
) -> Option<(i32, i32)> {
    None
}

/// Grow-only framebuffer resize; no framebuffer is served off Linux to grow.
pub async fn grow_framebuffer(_w: i32, _h: i32) -> bool {
    false
}

/// The root window's realized size; no root is served off Linux, so the caller's fallback
/// is the answer.
pub async fn read_realized_root(fallback: (i32, i32)) -> (i32, i32) {
    fallback
}

/// Whether this X server lets a display be added as an output of its own; no X server runs
/// off Linux, which leaves the logical-monitor layout in charge on Linux and nothing to
/// offer off it.
pub async fn has_pluggable_outputs() -> bool {
    false
}

/// Lay every display out as an output of its own; no outputs are served off Linux, so the
/// caller falls back as it does where the server has no such outputs.
pub async fn apply_output_layout(
    _layouts: &Layouts,
    _total_w: i32,
    _total_h: i32,
    _refresh: Option<f64>,
) -> bool {
    false
}

/// Drive the server into an extended desktop covering `layouts`; no layout is served off
/// Linux, so the caller drops the secondary it could not place, the answer a Linux build
/// gives a server that cannot extend the desktop.
pub async fn apply_extended_layout(
    _layouts: &mut Layouts,
    _total_w: i32,
    _total_h: i32,
    _refresh: Option<f64>,
) -> bool {
    false
}

/// Return the server to the one display it started with; nothing is laid out off Linux,
/// so there is nothing to retire.
pub async fn retire_displays() {}

/// Drop the retained X connection, for a caller that changed `DISPLAY`; no connection is
/// held off Linux.
pub fn drop_connection() {}
