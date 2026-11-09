/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The resize and layout entry points both transports call: native RandR first, on the
//! retained connection off the runtime's workers, with the xrandr subprocess chain as the
//! fallback each one degrades to when the native call fails. An extended desktop goes to the pluggable outputs where the server
//! offers them (`apply_output_layout`) and to the RandR 1.5 logical monitors anywhere else
//! (`monitors::apply_monitor_layout`).

use std::time::Duration;

use log::{error, info, warn};

use super::geometry::fit_res;
use super::x11::{self, on_x11};
use super::xrandr::{self, MAX_HEIGHT, MAX_WIDTH, parse_wh, run_xrandr};
use super::{DisplayError, Layouts};

/// How long the desktop is given to take in a moved primary before a display is plugged
/// in beside it. A toolkit reads the outputs when it handles the event, not when the event
/// was sent, so an arrangement replaced sooner is never seen.
const OUTPUT_SETTLE: Duration = Duration::from_millis(500);

/// Current and fitted resolution information for the first connected output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolutionInfo {
    /// The root's size as `WxH`.
    pub current: String,
    /// The request fitted into the 8K ceiling.
    pub fitted: String,
    /// The output's `WxH` mode names, sorted.
    pub modes: Vec<String>,
    /// The ceiling as `WxH`.
    pub max: String,
    /// The output's name, `None` when the server has no connected output, in which case
    /// `current` is still the root's size.
    pub output: Option<String>,
    /// The size the output's CRTC shows as `WxH`, which can be smaller than the root after a
    /// framebuffer grow; `None` where the output drives no CRTC or there is no output.
    pub shown: Option<String>,
}

/// Current/fitted resolution info for the first connected output: native RandR query
/// first, xrandr parse as fallback.
pub async fn get_new_res(res_str: &str) -> ResolutionInfo {
    let query = match on_x11(|x| x.query_randr()).await {
        Ok(q) => q,
        Err(e) => {
            info!(target: "display", "Native RandR query failed ({e}); using xrandr fallback.");
            return xrandr::get_new_res_xrandr(res_str).await;
        }
    };
    let current = format!("{}x{}", query.current.0, query.current.1);
    let Some(output) = query.output_name else {
        return ResolutionInfo {
            current,
            fitted: res_str.to_string(),
            modes: Vec::new(),
            max: res_str.to_string(),
            output: None,
            shown: None,
        };
    };
    let fitted = match parse_wh(res_str) {
        Some((w, h)) => {
            let (fw, fh) = fit_res(w, h, MAX_WIDTH, MAX_HEIGHT);
            format!("{fw}x{fh}")
        }
        None => {
            error!(target: "display", "Invalid resolution format for fitting: {res_str}");
            res_str.to_string()
        }
    };
    ResolutionInfo {
        current,
        fitted,
        modes: query.resolutions,
        max: format!("{MAX_WIDTH}x{MAX_HEIGHT}"),
        output: Some(output),
        shown: query.shown.map(|(w, h)| format!("{w}x{h}")),
    }
}

/// Ensure a `res_str` mode is attached to the connected output, at the refresh
/// `target_refresh` makes of `refresh` (the stream's frame rate). False leaves the caller
/// to its subprocess fallback.
pub async fn ensure_mode(res_str: &str, refresh: Option<f64>) -> bool {
    let Some((w, h)) = parse_wh(res_str) else {
        info!(target: "display", "Native RandR ensure-mode for '{res_str}' failed (invalid resolution).");
        return false;
    };
    match on_x11(move |x| x.ensure_mode(w, h, refresh)).await {
        Ok(()) => true,
        Err(e) => {
            info!(target: "display", "Native RandR ensure-mode for '{res_str}' failed ({e}).");
            false
        }
    }
}

/// Keep every connected output at the refresh `target_refresh` makes of `refresh`, the
/// stream's new frame rate, where the rate changes without a resize. Each output keeps its
/// geometry and position and takes a mode of the same size at the new refresh, so the
/// screen, the monitors, and every capture stay as they are.
pub async fn refresh_output_mode(refresh: Option<f64>) {
    let fps = refresh.unwrap_or(0.0);
    match on_x11(move |x| x.refresh_output_mode(refresh)).await {
        Ok(rates) if !rates.is_empty() => {
            let listed: Vec<String> = rates.iter().map(|r| format!("{r:.2}")).collect();
            info!(target: "display", "Display refresh set to {} Hz for a {fps} fps stream.", listed.join(", "));
        }
        Ok(_) => {}
        Err(e) => warn!(target: "display", "Display refresh not changed for {fps} fps ({e})."),
    }
}

/// Resize the display to `res_str` (such as `2560x1280`).
///
/// Native RandR first (`X11::resize_on_display`: the mode chosen by geometry and refresh,
/// `refresh` being the stream's frame rate, and made from CVT-RB timings when absent), with
/// the xrandr/cvt subprocess chain as fallback. `output_size` is the rectangle the output
/// shows when it is narrower than the screen, which only the logical-monitor layout asks
/// for. A server with no connected output has no mode to set, but its framebuffer is still
/// sized.
///
/// Returns the realized size -- CVT cell alignment may make it wider than requested -- or
/// `None` on failure. Callers must capture and report the realized size, not the request.
pub async fn resize_display(
    res_str: &str,
    refresh: Option<f64>,
    output_size: Option<(i32, i32)>,
) -> Option<(i32, i32)> {
    let Some((w_req, h_req)) = parse_wh(res_str) else {
        info!(target: "display", "Native RandR resize for '{res_str}' failed (invalid resolution); falling back to xrandr.");
        return xrandr::resize_display_xrandr(res_str, refresh).await;
    };
    match on_x11(move |x| x.resize_on_display(w_req, h_req, refresh, output_size)).await {
        Ok((w, h, rate)) => {
            info!(target: "display", "Successfully applied RandR mode '{res_str}' ({w}x{h} at {rate:.2} Hz).");
            Some((w, h))
        }
        Err(DisplayError::NoOutput) => {
            // No mode to set, but the framebuffer itself may still be sized.
            match on_x11(move |x| x.set_screen_size(w_req, h_req)).await {
                Ok((w, h)) if (w, h) == (w_req, h_req) => {
                    info!(target: "display", "No connected RandR output; sized the framebuffer to {w}x{h}.");
                    Some((w, h))
                }
                Ok((w, h)) => {
                    info!(target: "display", "No connected RandR output; the framebuffer stays at {w}x{h} for '{res_str}'.");
                    None
                }
                Err(e) => {
                    info!(
                        target: "display",
                        "No connected RandR output to size for '{res_str}' and the framebuffer cannot be resized ({e}); the X server keeps its size."
                    );
                    None
                }
            }
        }
        Err(e) => {
            info!(target: "display", "Native RandR resize for '{res_str}' failed ({e}); falling back to xrandr.");
            xrandr::resize_display_xrandr(res_str, refresh).await
        }
    }
}

/// Grow-only framebuffer resize, used before live captures re-target so no region ever
/// lies outside the root: native first, `xrandr --fb` fallback.
pub async fn grow_framebuffer(w: i32, h: i32) -> bool {
    match on_x11(move |x| x.grow_screen(w, h)).await {
        Ok(()) => true,
        Err(e) => {
            info!(target: "display", "Native framebuffer grow failed ({e}); using xrandr fallback.");
            run_xrandr(&["--fb", &format!("{w}x{h}")], "grow framebuffer").await
        }
    }
}

/// The root window's realized size, or `fallback` when it cannot be read.
pub async fn read_realized_root(fallback: (i32, i32)) -> (i32, i32) {
    let info = get_new_res("1x1").await;
    parse_wh(&info.current.to_lowercase().replace(' ', "")).unwrap_or(fallback)
}

/// Whether this X server lets a display be added as an output of its own; false on any
/// failure, which leaves the logical-monitor layout in charge.
pub async fn has_pluggable_outputs() -> bool {
    on_x11(|x| x.has_pluggable_outputs()).await.unwrap_or(false)
}

/// Lay every display out as an output of its own, where the server offers pluggable outputs
/// (`X11::apply_output_layout`), each at a mode keeping `refresh`, the stream's frame rate,
/// as `resize_display` does. A layout that adds a display and moves the primary is
/// published in two steps, the move first (`geometry::output_layout_stage`).
///
/// Returns true when the arrangement is in place. False -- the server has no such outputs,
/// not enough of them, or refused -- leaves the caller to the logical-monitor layout.
pub async fn apply_output_layout(
    layouts: &Layouts,
    total_w: i32,
    total_h: i32,
    refresh: Option<f64>,
) -> bool {
    if !has_pluggable_outputs().await {
        return false;
    }
    let attempt = async {
        let staged = layouts.clone();
        let stage = on_x11(move |x| x.output_stage(&staged)).await?;
        if let Some(stage) = stage {
            on_x11(move |x| x.apply_output_layout(&stage, total_w, total_h, refresh)).await?;
            tokio::time::sleep(OUTPUT_SETTLE).await;
        }
        let final_layout = layouts.clone();
        on_x11(move |x| x.apply_output_layout(&final_layout, total_w, total_h, refresh)).await
    };
    match attempt.await {
        Ok(()) => true,
        Err(e) => {
            warn!(target: "display", "Could not lay the displays out as outputs ({e}); using logical monitors.");
            if let Err(e) = on_x11(|x| x.retire_outputs()).await {
                warn!(target: "display", "Could not retire the secondary outputs ({e}).");
            }
            false
        }
    }
}

/// Drive the server into an extended desktop covering `layouts`.
///
/// Every display becomes an output of its own where the server offers pluggable outputs
/// (`apply_output_layout`); anywhere else they become logical monitors over its one output
/// (`monitors::apply_monitor_layout`). Either way the modes keep `refresh`, the stream's
/// frame rate, as `resize_display` does.
///
/// Returns true when the layout is in place. On the logical-monitor path `layouts` is
/// fitted in place to the root the server realized, so the caller reads the rectangles
/// back rather than reusing what it passed.
pub async fn apply_extended_layout(
    layouts: &mut Layouts,
    total_w: i32,
    total_h: i32,
    refresh: Option<f64>,
) -> bool {
    if apply_output_layout(layouts, total_w, total_h, refresh).await {
        return true;
    }
    super::monitors::apply_monitor_layout(layouts, total_w, total_h, refresh).await
}

/// Return the server to the one display it started with: the secondary outputs unplugged
/// where displays are outputs, and every logical monitor a fallback layout defined deleted.
/// The screen keeps its size, which the resize that follows a teardown sets.
pub async fn retire_displays() {
    if let Err(e) = on_x11(|x| x.retire_outputs()).await {
        warn!(target: "display", "Could not retire the secondary outputs ({e}).");
    }
    super::monitors::clear_selkies_monitors().await;
}

/// Drop the retained X connection, for a caller that changed `DISPLAY`.
pub fn drop_connection() {
    x11::drop_connection();
}
