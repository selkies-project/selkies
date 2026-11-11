/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The X11 display paths for a server that cannot give a display its own output.
//!
//! `resize` lays an extended desktop out as real RandR outputs wherever the X server offers
//! pluggable ones. This module is everything it falls back to where that is not available,
//! and nothing here runs on a server where it is:
//!
//! - An extended desktop as RandR 1.5 logical monitors over the one output a framebuffer
//!   server has (`apply_monitor_layout`, `replace_selkies_monitors`). The whole set is
//!   swapped under one server grab, every monitor asks for the physical output because GTK
//!   realizes a monitor only where one is listed, whether the server lets them share it is
//!   read back rather than assumed, and the swap is announced on an output property because
//!   `RRSetMonitor` emits no RandR event of its own. Qt matches a monitor to its screen by
//!   name and announces a new geometry only for a monitor that is exactly one CRTC, which no
//!   display of several sharing the output is, so a display whose rectangle changes takes a
//!   monitor named for the new rectangle (`monitor_names`) and Qt replaces that one screen.
//!   A single display is shown whole by the output, its CRTC exactly the monitor rather than
//!   the framebuffer rounded up to the CVT cell (`resize_display`'s `output_size`).
//! - The window-manager restart that layout needs (`MultiMonitorWindowManager`): a manager
//!   that reads the monitor set only as it starts has to be restarted once to tile against
//!   it. A display that arrives as an output is a hotplug, which every manager follows, so
//!   there is nothing to restart there.
//! - A server whose driver reports no display device has no output to list and no screen of
//!   its own: there the monitors are the only screens the toolkits find, so they are
//!   published without one and clearing the layout leaves one covering the framebuffer
//!   (`restore_framebuffer_monitor`).
//!
//! It shares the retained connection and its lock, so the same rules hold: blocking X work
//! runs off the runtime's workers, and a mutation reads its result back before claiming
//! success.

use std::collections::{BTreeMap, BTreeSet};
use std::process::Stdio;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use log::{debug, error, info, warn};
use x11rb::protocol::randr::MonitorInfo;

use super::modes::{mode_timings, target_refresh};
use super::resize::{
    ResolutionInfo, ensure_mode, get_new_res, grow_framebuffer, has_pluggable_outputs,
    read_realized_root, resize_display,
};
use super::x11::{self, ModeSet, WindowRect, X11, applied_dpi, on_x11, with_x11};
use super::xrandr::{list_monitors_xrandr, parse_wh, run_xrandr};
use super::{DisplayError, Layout, Layouts, Rect};
use crate::core::PRIMARY_DISPLAY;

type Result<T> = std::result::Result<T, DisplayError>;

/// Whether this X server lets several logical monitors list the same physical output,
/// measured the first time a layout asks for it; `None` until then. RandR 1.5 has an output
/// belong to one monitor and servers before 21.1 enforce it (`set_selkies_layout`).
static OUTPUT_SHARED: Mutex<Option<bool>> = Mutex::new(None);

/// Bumped on the physical output to announce a monitor-set change.
const MONITOR_SERIAL_ATOM: &str = "_SELKIES_MONITOR_SERIAL";
static MONITOR_SERIAL: AtomicU32 = AtomicU32::new(0);

fn output_shared() -> Option<bool> {
    *OUTPUT_SHARED.lock().unwrap_or_else(|e| e.into_inner())
}

fn set_output_shared(value: bool) {
    *OUTPUT_SHARED.lock().unwrap_or_else(|e| e.into_inner()) = Some(value);
}

/// A live `selkies-*` monitor: its rectangle, whether it lists the output, and whether it
/// carries the primary flag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LiveMonitor {
    pub rect: Rect,
    pub has_output: bool,
    pub primary: bool,
}

/// The display id a `selkies-*` monitor name carries.
pub fn display_of(name: &str) -> &str {
    name.strip_prefix("selkies-")
        .unwrap_or(name)
        .split('@')
        .next()
        .unwrap_or("")
}

/// The monitor name each display of `layouts` is published under.
///
/// A new display takes `selkies-<id>`. One that stays keeps the name its live monitor has
/// while its rectangle is unchanged, or while it is the only display, whose CRTC shows
/// exactly its monitor so that Qt announces a change in place (`resize_display`'s
/// `output_size`). Any other display that moved or resized is named for its new rectangle,
/// `selkies-<id>@WxH+X+Y`: Qt matches a monitor to its screen by name and stores a new
/// geometry for a monitor that is not one CRTC without announcing it, so a Qt desktop would
/// keep drawing the display at the size and place it had, where a new name makes Qt
/// replace that screen. GTK keys its monitors by output and KWin its screens by CRTC, so
/// neither sees the name change.
pub fn monitor_names(
    layouts: &Layouts,
    live: &BTreeMap<String, LiveMonitor>,
) -> BTreeMap<String, String> {
    let kept: BTreeMap<&str, (&str, Rect)> = live
        .iter()
        .map(|(name, m)| (display_of(name), (name.as_str(), m.rect)))
        .collect();
    layouts
        .iter()
        .map(|(did, l)| {
            let name = match kept.get(did.as_str()) {
                Some((_, rect)) if *rect != l.rect() && layouts.len() > 1 => {
                    format!("selkies-{did}@{}x{}+{}+{}", l.w, l.h, l.x, l.y)
                }
                Some((name, _)) => name.to_string(),
                None => format!("selkies-{did}"),
            };
            (did.clone(), name)
        })
        .collect()
}

/// Whether the live `selkies-*` set already is what a publish would define: the same
/// rectangles, the primary flag on the primary, and the output where this server was
/// measured to keep it.
pub fn monitors_match(
    live: &BTreeMap<String, LiveMonitor>,
    desired: &BTreeMap<String, Rect>,
    share_output: bool,
    has_output: bool,
) -> bool {
    let live_rects: BTreeMap<&String, Rect> = live.iter().map(|(n, m)| (n, m.rect)).collect();
    let desired_rects: BTreeMap<&String, Rect> = desired.iter().map(|(n, r)| (n, *r)).collect();
    if live_rects != desired_rects {
        return false;
    }
    if live
        .iter()
        .any(|(name, m)| display_of(name) == PRIMARY_DISPLAY && !m.primary)
    {
        return false;
    }
    if !has_output {
        return live.values().all(|m| !m.has_output);
    }
    live.iter()
        .all(|(name, m)| m.has_output == (share_output || display_of(name) == PRIMARY_DISPLAY))
}

/// Build the `RRSetMonitor` request for logical monitor `name`.
///
/// `take_output` lists the (single) connected physical output on this monitor, and a server
/// that has none lists nothing whatever it asks for. GTK3's X11 backend realizes a
/// GdkMonitor only for a RandR monitor that carries a live output, so a monitor without one
/// is invisible to every GTK app and their desktops paint and tile short of that region --
/// which is why every monitor asks for it. Whether the server lets them all keep it is a
/// property of the server (`set_selkies_layout`), so a caller that has measured a refusal
/// passes false for the rest.
///
/// The primary monitor carries the RandR primary flag: the WM tiles panels against it, and
/// the same-set no-op in `replace_selkies_monitors_sync` reads it back to prove the live set
/// already matches. The flag is the only mark of the primary in a multi-display set, because
/// Qt also takes a monitor listing the primary output for the primary screen, which every
/// one of these does.
fn monitor_info(
    x: &mut X11,
    out_id: Option<u32>,
    name: &str,
    rect: Rect,
    take_output: bool,
) -> Result<MonitorInfo> {
    let (mx, my, w, h) = rect;
    let dpi = applied_dpi().unwrap_or(96) as f64;
    Ok(MonitorInfo {
        name: x.atom(name)?,
        primary: display_of(name) == PRIMARY_DISPLAY,
        automatic: false,
        x: mx as i16,
        y: my as i16,
        width: w as u16,
        height: h as u16,
        width_in_millimeters: ((w as f64 * 25.4 / dpi).round() as u32).max(1),
        height_in_millimeters: ((h as f64 * 25.4 / dpi).round() as u32).max(1),
        outputs: match (take_output, out_id) {
            (true, Some(o)) => vec![o],
            _ => Vec::new(),
        },
    })
}

/// Live `selkies-*` monitors by name.
fn selkies_monitors(x: &X11) -> Result<BTreeMap<String, LiveMonitor>> {
    let mut monitors = BTreeMap::new();
    for m in x.monitors()? {
        let Ok(name) = x.monitor_name(&m) else {
            continue;
        };
        if name.starts_with("selkies-") {
            monitors.insert(
                name,
                LiveMonitor {
                    rect: (m.x as i32, m.y as i32, m.width as i32, m.height as i32),
                    has_output: !m.outputs.is_empty(),
                    primary: m.primary,
                },
            );
        }
    }
    Ok(monitors)
}

/// Verify every expected logical monitor is defined with its geometry: a request that was
/// carried out is not one the server honored.
fn verify_monitors(x: &X11, expected: &BTreeMap<String, Rect>) -> Result<()> {
    let mut actual = BTreeMap::new();
    for m in x.monitors()? {
        if let Ok(name) = x.monitor_name(&m) {
            actual.insert(
                name,
                (m.x as i32, m.y as i32, m.width as i32, m.height as i32),
            );
        }
    }
    for (name, rect) in expected {
        if actual.get(name) != Some(rect) {
            return Err(DisplayError::Refused(format!(
                "monitor '{name}' is {:?} after define, wanted {rect:?}",
                actual.get(name)
            )));
        }
    }
    Ok(())
}

/// Delete every `selkies-*` monitor; the caller holds the connection.
///
/// For the output layout, which takes over from a monitor layout a previous run or a
/// fallback left behind: the server derives a monitor from every active CRTC only once none
/// is defined by hand.
pub(crate) fn drop_selkies_monitors(x: &mut X11) -> Result<()> {
    for name in selkies_monitors(x)?.keys() {
        x.delete_monitor(name)?;
    }
    Ok(())
}

/// Emit the RandR event a toolkit re-reads its monitor set on.
///
/// `RRSetMonitor` emits no RandR event: the server sends core ConfigureNotify on the root,
/// which GTK3 discards wherever RandR 1.3 is present, re-reading its monitors only for
/// RRScreenChangeNotify or RRNotify. So a swap that does not resize the framebuffer never
/// reaches a running desktop, which keeps painting and constraining windows against the
/// monitors it last saw. An output property is the one RRNotify carrying no geometry,
/// physical size, or CRTC of its own, and the server emits it even for an unchanged value.
///
/// A server with no output carries no property to bump, so nothing is sent there; the
/// framebuffer resize a swap on such a server is part of emits its own event, which is what
/// makes the new set visible. Advisory, and synced here so a failure surfaces inside it: a
/// set that is right but unannounced is no reason to report the swap as failed.
fn announce_monitor_change_sync(x: &mut X11, out_id: Option<u32>) {
    let Some(out_id) = out_id else {
        return;
    };
    let serial = (MONITOR_SERIAL.fetch_add(1, Ordering::Relaxed) + 1) & 0x7FFF_FFFF;
    if let Err(e) = x
        .set_output_property_u32(out_id, MONITOR_SERIAL_ATOM, serial)
        .and_then(|_| x.sync())
    {
        debug!(target: "display", "Could not announce the monitor-set change ({e}).");
    }
}

/// Define the whole `selkies-*` set from scratch; returns what survived.
///
/// RandR 1.5 has an output belong to one logical monitor: defining a monitor over an output
/// takes it from whichever monitor held it and deletes that monitor once it is left with
/// none. Servers before 21.1 do exactly that, so listing the one physical output on every
/// display would leave the last display alone. `share_output` asks for it on all of them
/// anyway, because a server that does not enforce the rule is the only way several displays
/// become GdkMonitors at once (`monitor_info`); the caller compares the returned set against
/// what it asked for and repeats with false, which leaves the output on the primary and the
/// rest of the displays invisible to GTK apps.
fn set_selkies_layout(
    x: &mut X11,
    out_id: Option<u32>,
    ordered: &[(&String, &Layout)],
    share_output: bool,
    names: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, LiveMonitor>> {
    drop_selkies_monitors(x)?;
    let mut take_output = true;
    for (did, l) in ordered {
        let info = monitor_info(x, out_id, &names[*did], l.rect(), take_output)?;
        x.set_monitor(info)?;
        take_output = share_output;
    }
    x.sync()?;
    selkies_monitors(x)
}

/// The layouts with the primary first.
fn primary_first(layouts: &Layouts) -> Vec<(&String, &Layout)> {
    let mut ordered: Vec<(&String, &Layout)> = layouts.iter().collect();
    ordered.sort_by_key(|(did, _)| *did != PRIMARY_DISPLAY);
    ordered
}

/// Blocking swap of all `selkies-*` logical monitors to exactly `layouts`.
///
/// The whole swap runs under one X server grab: `RRSetMonitor` cannot replace an existing
/// name (BadValue), so a delete gap is unavoidable, and the grab makes it invisible: every
/// event a window manager acts on is delivered after the final set is in place, so it never
/// tiles against a monitor-less or half-defined screen. The set alone announces nothing a
/// toolkit listens to, so `announce_monitor_change_sync` closes the grab. Foreign monitors
/// are left untouched. A request matching the live set returns without touching the server:
/// even that swap costs a delete+create and a re-tile. A live set whose outputs sit
/// differently than this server was measured to allow never counts as a match: monitors
/// outlive the client that set them, and a stale outputless set at the right geometry is
/// invisible to GTK apps.
///
/// `mode`, prepared on this connection, is applied first in the same grab, and an error
/// applying it leaves the monitors as they were (`replace_selkies_monitors_at`).
fn replace_selkies_monitors_sync(
    x: &mut X11,
    layouts: &Layouts,
    mode: Option<&ModeSet>,
) -> Result<()> {
    let out_id = x.first_connected_output()?;
    let live = selkies_monitors(x)?;
    let names = monitor_names(layouts, &live);
    let desired: BTreeMap<String, Rect> = layouts
        .iter()
        .map(|(did, l)| (names[did].clone(), l.rect()))
        .collect();
    let ordered = primary_first(layouts);
    let share = output_shared() != Some(false);
    let swap = !monitors_match(&live, &desired, share, out_id.is_some());
    if !swap && mode.is_none() {
        return Ok(());
    }
    x.grabbed(|x| {
        if let Some(set) = mode {
            x.apply_mode_set(set)?;
        }
        if !swap {
            return Ok(());
        }
        let live = set_selkies_layout(x, out_id, &ordered, share, &names)?;
        let live_names: BTreeSet<&String> = live.keys().collect();
        let wanted: BTreeSet<&String> = desired.keys().collect();
        if share && live_names != wanted {
            set_output_shared(false);
            info!(
                target: "display",
                "This X server keeps a logical monitor's output to itself, so only the primary display becomes a monitor toolkits can see; the rest of the desktop paints and tiles as if they were not there."
            );
            set_selkies_layout(x, out_id, &ordered, false, &names)?;
        } else if share {
            set_output_shared(true);
        }
        // Qt takes any monitor listing the primary output for the primary screen, and these
        // monitors all list the one output, so with several displays no output is primary
        // and the monitor flag alone names the primary.
        if let Some(out) = out_id {
            x.set_output_primary(if layouts.len() == 1 { out } else { 0 })?;
        }
        announce_monitor_change_sync(x, out_id);
        Ok(())
    })?;
    x.sync()?;
    verify_monitors(x, &desired)
}

/// Tell the toolkits already running that the monitor set changed. The grab-protected
/// replace announces its own; this is for the routes that reach a final set some other way.
pub async fn announce_monitor_change() {
    let announced = on_x11(|x| {
        let out = x.first_connected_output()?;
        announce_monitor_change_sync(x, out);
        Ok(())
    })
    .await;
    if let Err(e) = announced {
        debug!(target: "display", "Could not announce the monitor-set change ({e}).");
    }
}

/// Swap the `selkies-*` logical monitor set to exactly `layouts`: native grab-protected
/// replace first, per-monitor xrandr fallback second (the fallback exposes transient states
/// to the WM, but ends at the same result). Returns true when the final monitor set is in
/// place.
pub async fn replace_selkies_monitors(layouts: &Layouts, screen_name: Option<&str>) -> bool {
    if layouts.is_empty() {
        clear_selkies_monitors().await;
        return true;
    }
    let owned = layouts.clone();
    match on_x11(move |x| replace_selkies_monitors_sync(x, &owned, None)).await {
        Ok(()) => return true,
        Err(e) => {
            info!(target: "display", "Native monitor replace failed ({e}); using xrandr fallback.")
        }
    }
    clear_selkies_monitors().await;
    let mut ok = true;
    let mut take_output = true;
    let names = monitor_names(layouts, &BTreeMap::new());
    for (did, l) in primary_first(layouts) {
        ok &= set_logical_monitor(&names[did], l.rect(), take_output, screen_name).await;
        take_output = output_shared() != Some(false);
    }
    designate_primary_output(screen_name).await;
    announce_monitor_change().await;
    ok
}

/// Swap the `selkies-*` set to `layouts` and set the framebuffer to `w` by `h` in one server
/// grab, natively. False leaves both to `replace_selkies_monitors` and `resize_display`, whose
/// xrandr fallbacks are clients of their own, which a grabbed server does not answer.
///
/// GTK takes the screen's size from its monitors, and a desktop that reads the CRTCs when GTK
/// reports a change (MATE's panel, in an idle callback) lays out for the mode it finds. A swap
/// and a mode set in grabs of their own leave a gap in which that read finds the new monitor
/// over the old mode, and the mode set changes no monitor, so nothing is reported again: the
/// panel stays laid out for the old mode. In one grab, every read a client makes on either
/// change is answered with both in place. The mode goes in first, so one the server refuses
/// leaves the monitors as they were.
async fn replace_selkies_monitors_at(
    layouts: &Layouts,
    w: i32,
    h: i32,
    refresh: Option<f64>,
    output_size: Option<(i32, i32)>,
) -> bool {
    if layouts.is_empty() {
        return false;
    }
    let owned = layouts.clone();
    let done = on_x11(move |x| {
        let set = x.prepare_mode_set(w, h, refresh, output_size)?;
        replace_selkies_monitors_sync(x, &owned, Some(&set))?;
        x.mode_set_realized(&set)
    })
    .await;
    match done {
        Ok((rw, rh, rate)) => {
            info!(target: "display", "Successfully applied RandR mode '{w}x{h}' ({rw}x{rh} at {rate:.2} Hz).");
            true
        }
        Err(e) => {
            info!(target: "display", "Native mode set with the monitor swap failed ({e}); swapping and setting the mode apart.");
            false
        }
    }
}

/// Define/replace logical monitor `name` over `rect`: native RandR 1.5 first, `xrandr
/// --setmonitor` fallback. `take_output` lists the physical output on the monitor
/// (`monitor_info`); the fallback also goes outputless when no output name is known.
pub async fn set_logical_monitor(
    name: &str,
    rect: Rect,
    take_output: bool,
    screen_name: Option<&str>,
) -> bool {
    let owned = name.to_string();
    let native = on_x11(move |x| {
        let out_id = x.first_connected_output()?;
        let info = monitor_info(x, out_id, &owned, rect, take_output)?;
        x.set_monitor(info)?;
        x.sync()?;
        verify_monitors(x, &[(owned.clone(), rect)].into_iter().collect())
    })
    .await;
    match native {
        Ok(()) => return true,
        Err(e) => {
            info!(target: "display", "Native set-monitor '{name}' failed ({e}); using xrandr fallback.")
        }
    }
    let (x, y, w, h) = rect;
    let geometry = format!("{w}/0x{h}/0+{x}+{y}");
    let output = match (take_output, screen_name) {
        (true, Some(s)) => s,
        _ => "none",
    };
    run_xrandr(
        &["--setmonitor", name, &geometry, output],
        &format!("set logical monitor {name}"),
    )
    .await
}

/// Delete logical monitor `name`: native first, xrandr fallback. Deleting an absent monitor
/// counts as failure on both paths.
pub async fn delete_logical_monitor(name: &str) -> bool {
    let owned = name.to_string();
    match on_x11(move |x| {
        x.delete_monitor(&owned)?;
        x.sync()
    })
    .await
    {
        Ok(()) => true,
        Err(e) => {
            debug!(target: "display", "Native delete-monitor '{name}' failed ({e}); using xrandr fallback.");
            run_xrandr(&["--delmonitor", name], &format!("delete monitor {name}")).await
        }
    }
}

/// Flag the connected physical output primary so the WM anchors panels and new windows to
/// it: native first, xrandr fallback. A no-op on a server with no output, where the logical
/// monitors carry the primary flag and nothing else can.
pub async fn designate_primary_output(screen_name: Option<&str>) -> bool {
    let native = on_x11(|x| {
        if let Some(out) = x.first_connected_output()? {
            x.set_output_primary(out)?;
            x.sync()?;
        }
        Ok(())
    })
    .await;
    match native {
        Ok(()) => true,
        Err(e) => {
            info!(target: "display", "Native set-output-primary failed ({e}); using xrandr fallback.");
            match screen_name {
                Some(s) => {
                    run_xrandr(&["--output", s, "--primary"], "designate primary output").await
                }
                None => false,
            }
        }
    }
}

/// Names of all RandR logical monitors: native query first, `xrandr --listmonitors` parse
/// as fallback.
pub async fn list_logical_monitors() -> Vec<String> {
    match on_x11(|x| {
        let mut names = Vec::new();
        for m in x.monitors()? {
            if let Ok(name) = x.monitor_name(&m) {
                names.push(name);
            }
        }
        Ok(names)
    })
    .await
    {
        Ok(names) => names,
        Err(e) => {
            info!(target: "display", "Native monitor list failed ({e}); using xrandr fallback.");
            list_monitors_xrandr().await
        }
    }
}

/// Names of the logical monitors this software created (`selkies-*`).
pub async fn list_selkies_monitors() -> Vec<String> {
    list_logical_monitors()
        .await
        .into_iter()
        .filter(|n| n.contains("selkies-"))
        .collect()
}

/// Define one monitor covering the framebuffer, where the server has no output. Returns
/// whether one was defined: a server whose driver exposes an output keeps a screen of its
/// own once the layout monitors are gone, so nothing is defined there.
async fn restore_framebuffer_monitor() -> bool {
    match on_x11(|x| {
        if x.first_connected_output()?.is_some() {
            return Ok(false);
        }
        let (w, h) = x.root_size()?;
        let info = monitor_info(x, None, "selkies-primary", (0, 0, w, h), false)?;
        x.set_monitor(info)?;
        x.sync()?;
        Ok(true)
    })
    .await
    {
        Ok(defined) => defined,
        Err(e) => {
            info!(target: "display", "Could not define a monitor over the framebuffer ({e}).");
            false
        }
    }
}

/// The manager's clients and the displays' rectangles as the logical monitors have them,
/// read before a layout is swapped in.
#[derive(Clone, Debug, Default)]
pub struct WindowSnapshot {
    pub windows: Vec<WindowRect>,
    pub rects: BTreeMap<String, Rect>,
}

/// Each display's rectangle as the logical monitors have it; with none defined, the primary
/// is the framebuffer.
fn display_rects(x: &mut X11) -> Result<BTreeMap<String, Rect>> {
    let mut rects: BTreeMap<String, Rect> = selkies_monitors(x)?
        .iter()
        .map(|(name, m)| (display_of(name).to_string(), m.rect))
        .collect();
    if rects.is_empty() {
        let (w, h) = x.root_size()?;
        rects.insert(PRIMARY_DISPLAY.into(), (0, 0, w, h));
    }
    Ok(rects)
}

/// The manager's clients and the displays' rectangles, read before a layout change moves
/// anything; empty where the X server cannot be read.
pub async fn window_snapshot() -> WindowSnapshot {
    on_x11(|x| {
        Ok(WindowSnapshot {
            windows: x.client_windows()?,
            rects: display_rects(x)?,
        })
    })
    .await
    .unwrap_or_default()
}

/// Move the windows of `snapshot` with their displays into `layouts`, once the framebuffer
/// holds the layout: a manager constrains a move against the screen it has, so one asked
/// between the monitor swap and the resize that follows it is pushed back inside the old
/// framebuffer.
pub async fn follow_display_moves(snapshot: &WindowSnapshot, layouts: &Layouts) {
    let after: BTreeMap<String, Rect> =
        layouts.iter().map(|(d, l)| (d.clone(), l.rect())).collect();
    follow(snapshot.clone(), after).await;
}

async fn follow(snapshot: WindowSnapshot, after: BTreeMap<String, Rect>) {
    let moved =
        on_x11(move |x| x.follow_display_moves(&snapshot.windows, &snapshot.rects, &after)).await;
    if let Err(e) = moved {
        debug!(target: "display", "Windows not moved with their displays ({e}).");
    }
    x11::seat_desktop_windows();
}

/// Delete every logical monitor this software created (`selkies-*`).
///
/// A server with no connected output has no screen of its own to fall back to: its monitors
/// are the only ones the toolkits see, so one covering the framebuffer takes the layout's
/// place rather than leaving the desktop with nowhere to put a window. The primary's
/// windows follow it back to the origin, and a departed display's onto it.
pub async fn clear_selkies_monitors() {
    let names = list_selkies_monitors().await;
    let snapshot = if names.is_empty() {
        None
    } else {
        Some(window_snapshot().await)
    };
    for name in &names {
        delete_logical_monitor(name).await;
    }
    let restored = restore_framebuffer_monitor().await;
    if let Some(snapshot) = snapshot
        && let Some(&(_, _, w, h)) = snapshot.rects.get(PRIMARY_DISPLAY)
    {
        // With the monitors gone the primary is the framebuffer at the origin.
        let after: BTreeMap<String, Rect> = [(PRIMARY_DISPLAY.to_string(), (0, 0, w, h))]
            .into_iter()
            .collect();
        follow(snapshot, after).await;
    }
    if !names.is_empty() && !restored {
        announce_monitor_change().await;
    }
}

/// Whether the extended layout needs a mode-set: the root is not `total_w` by `total_h`, or it
/// is but the output's CRTC shows less of it. A framebuffer grown ahead of live captures
/// (`grow_framebuffer`) already has the layout's size while the output keeps its old mode, and
/// the X server keeps the pointer inside what the outputs show, so a display beyond that mode
/// could be captured but not pointed at. A CRTC wider than asked, as CVT alignment can leave
/// it, needs nothing.
fn needs_mode_set(info: &ResolutionInfo, total_w: i32, total_h: i32) -> bool {
    let root = info.current.to_lowercase().replace(' ', "");
    root != format!("{total_w}x{total_h}")
        || info
            .shown
            .as_deref()
            .and_then(parse_wh)
            .is_some_and(|(w, h)| w < total_w || h < total_h)
}

/// Drive the server into an extended-desktop framebuffer covering `layouts`.
///
/// Ensures the total mode exists at `refresh`, the stream's frame rate (`resize_display`),
/// sizes the framebuffer, and defines one `selkies-<id>` logical monitor per display so
/// window managers tile against the per-display regions.
///
/// The monitors and the mode go in under one server grab (`replace_selkies_monitors_at`),
/// the monitors at their final rectangles: window managers re-tile maximized windows on every
/// root ConfigureNotify, so no stimulus may ever expose a monitor-less or partial set, nor a
/// monitor over a mode the screen does not have yet. Where that native path fails, the swap
/// and the resize run apart, the swap first, each with its fallback. A server that refuses
/// runtime mode creation may still honor a plain framebuffer grow
/// (`RRSetScreenSize`): the output keeps its mode while captures and pointer warps address
/// the enlarged root. The server, not the request, is the authority on the realized
/// geometry, and it can report success while leaving the root short, so the layout is fitted
/// to what is really there before any capture is pointed at it; where fitting moved a
/// display the monitors are all redefined at the fitted rectangles (a dropped display's
/// monitor disappears with the swap), while a root that merely came back larger than asked
/// leaves them alone, since every swap makes window managers re-tile. The windows follow
/// their displays last of all, once the framebuffer holds the layout.
///
/// Returns true when the framebuffer and monitors were set. `layouts` is fitted in place to
/// the root the server actually produced, so the caller must read the rectangles back
/// rather than reuse the ones it passed in. False when nothing could be laid out; the
/// monitors are torn down.
pub async fn apply_monitor_layout(
    layouts: &mut Layouts,
    total_w: i32,
    total_h: i32,
    refresh: Option<f64>,
) -> bool {
    let total_mode = format!("{total_w}x{total_h}");
    let snapshot = window_snapshot().await;
    let info = get_new_res(&total_mode).await;
    let screen_name = info.output.clone();
    match &screen_name {
        None => {
            // No output means no mode to create or set, and the framebuffer alone is what
            // the resize below sizes; the monitors are still what gives the toolkits their
            // screens, so the layout goes on.
            info!(target: "display", "No connected RandR output on this X server; the desktop is laid out on the framebuffer alone.");
        }
        Some(screen) if !info.modes.contains(&total_mode) => {
            if !ensure_mode(&total_mode, refresh).await {
                let modeline =
                    mode_timings(total_w as u32, total_h as u32, target_refresh(refresh))
                        .modeline();
                let mut args = vec!["--newmode", total_mode.as_str()];
                args.extend(modeline.split_whitespace());
                run_xrandr(&args, "create mode").await;
                run_xrandr(&["--addmode", screen, &total_mode], "add mode").await;
            }
        }
        Some(_) => {}
    }
    let mode_set = needs_mode_set(&info, total_w, total_h);
    // A lone display is shown whole by the output, its CRTC exactly its monitor: Qt
    // announces a monitor's new geometry only where the two agree, and the framebuffer is
    // the width rounded up to the CVT cell.
    let output_size = match (layouts.len(), layouts.get(PRIMARY_DISPLAY)) {
        (1, Some(l)) => Some((l.w, l.h)),
        _ => None,
    };
    let together = mode_set
        && screen_name.is_some()
        && replace_selkies_monitors_at(layouts, total_w, total_h, refresh, output_size).await;
    if !together {
        if !replace_selkies_monitors(layouts, screen_name.as_deref()).await {
            clear_selkies_monitors().await;
            return false;
        }
        if mode_set
            && resize_display(&total_mode, refresh, output_size)
                .await
                .is_none()
            && !grow_framebuffer(total_w, total_h).await
        {
            error!(
                target: "display",
                "Neither a mode-set nor a framebuffer grow reached {total_mode}; fitting the layout to whatever the root realized."
            );
        }
    }
    let (realized_w, realized_h) = read_realized_root((total_w, total_h)).await;
    if (realized_w, realized_h) == (total_w, total_h) {
        follow_display_moves(&snapshot, layouts).await;
        return true;
    }
    warn!(
        target: "display",
        "Realized screen size {realized_w}x{realized_h} differs from target {total_mode}; fitting the display layouts to it."
    );
    let offsets: BTreeMap<String, (i32, i32)> = layouts
        .iter()
        .map(|(d, l)| (d.clone(), (l.x, l.y)))
        .collect();
    let fit = super::geometry::reconcile_realized_layout(layouts, realized_w, realized_h);
    if fit.reanchored {
        let (px, py) = offsets.get(PRIMARY_DISPLAY).copied().unwrap_or((0, 0));
        error!(
            target: "display",
            "Primary at +{px}+{py} does not fit the realized {realized_w}x{realized_h} root; re-anchored at the origin."
        );
    }
    for did in &fit.dropped {
        let (dx, dy) = offsets.get(did).copied().unwrap_or((0, 0));
        error!(
            target: "display",
            "Display '{did}' at +{dx}+{dy} does not fit the realized {realized_w}x{realized_h} root; dropping it. The X server must allow a framebuffer covering all displays (e.g. a larger Xvfb -screen) for extended layouts."
        );
    }
    for did in &fit.clamped {
        if let Some(l) = layouts.get(did) {
            warn!(target: "display", "Display '{did}': layout clamped to {}x{} inside the realized root.", l.w, l.h);
        }
    }
    if (!fit.dropped.is_empty() || fit.reanchored || !fit.clamped.is_empty())
        && !replace_selkies_monitors(layouts, screen_name.as_deref()).await
    {
        clear_selkies_monitors().await;
        return false;
    }
    follow_display_moves(&snapshot, layouts).await;
    true
}

/// Options that start a desktop session rather than a window manager: the hook that runs
/// the session's autostart, and the session-manager id the running instance registered.
/// Reusing them would launch every autostart application a second time and claim an id that
/// is already held.
pub const RESTART_DROPS_OPTIONS: [&str; 2] = ["--startup", "--sm-client-id"];

/// `command` reduced to what restarts the manager alone.
///
/// The command line a session started its manager with carries the autostart hook (Openbox's
/// `--startup openbox-autostart`), and a restart that keeps it runs the desktop's autostart
/// again: a terminal, a panel, and everything else the session opens, once per restart. The
/// manager is being restarted only to re-read the monitor set, so the session's own options
/// are dropped.
pub fn restart_command(command: &[String]) -> Vec<String> {
    let mut kept = Vec::new();
    let mut drop_value = false;
    for arg in command {
        if drop_value {
            drop_value = false;
            continue;
        }
        let option = arg.split_once('=').map(|(o, _)| o).unwrap_or(arg);
        if RESTART_DROPS_OPTIONS.contains(&option) {
            drop_value = !arg.contains('=');
            continue;
        }
        kept.push(arg.clone());
    }
    kept
}

/// Window managers measured to read the RandR monitor set only as they start, by the
/// leading run of letters and digits of the name they advertise: a session extending onto a
/// second display restarts one of these so it reads the set the layout published. Others
/// follow monitor changes live, or never read the set at all (kwin_x11 builds its screens
/// from CRTCs, of which a framebuffer server has one).
pub const RESTART_TO_READ_MONITORS: [&str; 2] = ["openbox", "xfwm4"];

/// Whether the window manager advertising `wm_name` is the one the command `command_name`
/// starts. A window manager rarely advertises its binary's name: kwin_x11 answers "KWin",
/// Openbox "Openbox". Both are reduced to their leading run of letters and digits, case
/// aside, and one has to begin with the other.
pub fn wm_name_matches(command_name: &str, wm_name: &str) -> bool {
    fn stem(name: &str) -> String {
        name.trim()
            .to_lowercase()
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect()
    }
    let (want, have) = (stem(command_name), stem(wm_name));
    !want.is_empty() && !have.is_empty() && (want.starts_with(&have) || have.starts_with(&want))
}

/// The command line process `pid` was started with, from `/proc`; empty when there is no
/// such process to read.
pub fn wm_command(pid: i32) -> Vec<String> {
    if pid <= 0 {
        return Vec::new();
    }
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|raw| {
            raw.split(|b| *b == 0)
                .filter(|a| !a.is_empty())
                .map(|a| String::from_utf8_lossy(a).into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// What the manager restart reads and does, so the decision runs against the live server
/// through `LiveWindowManagers` and against stand-ins in tests.
pub trait WindowManagerHost {
    /// Name of the running EWMH window manager, empty when undetectable.
    fn wm_name(&self) -> String;
    /// Process id the running EWMH window manager advertises, 0 when none.
    fn wm_pid(&self) -> i32;
    /// The command line process `pid` was started with.
    fn wm_command(&self, pid: i32) -> Vec<String>;
    /// Start `command` detached, so it outlives this process's session.
    fn spawn(&self, command: &[String]) -> std::io::Result<()>;
    /// Wait until the manager `name` starts is the one running, and not `replacing`.
    fn wait_for_wm(&self, name: &str, replacing: i32) -> bool;
}

/// The running X server's window managers.
pub struct LiveWindowManagers;

impl WindowManagerHost for LiveWindowManagers {
    fn wm_name(&self) -> String {
        with_x11(|x| x.wm_name()).unwrap_or_default()
    }

    fn wm_pid(&self) -> i32 {
        with_x11(|x| x.wm_pid()).unwrap_or(0)
    }

    fn wm_command(&self, pid: i32) -> Vec<String> {
        wm_command(pid)
    }

    fn spawn(&self, command: &[String]) -> std::io::Result<()> {
        use std::os::unix::process::CommandExt;
        let (program, args) = command
            .split_first()
            .ok_or_else(|| std::io::Error::other("empty command"))?;
        std::process::Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .map(drop)
    }

    fn wait_for_wm(&self, name: &str, replacing: i32) -> bool {
        wait_for_wm(self, name, replacing, Duration::from_secs(3))
    }
}

/// Wait until the window manager `name_substring` starts is the one running
/// (`wm_name_matches`) and, given the process id of the one it replaces, no longer that one.
///
/// Used after a WM `--replace` so layout changes are not applied while two window managers
/// hand over the selection: the incoming WM snapshots the monitor set it starts against, and
/// the outgoing one answers to the same name until it is gone. Blocking.
pub fn wait_for_wm(
    host: &dyn WindowManagerHost,
    name_substring: &str,
    replacing: i32,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if wm_name_matches(name_substring, &host.wm_name()) {
            let pid = host.wm_pid();
            if replacing == 0 || (pid != 0 && pid != replacing) {
                return true;
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// Restarts X11 window management once a session has two displays, where the manager
/// running needs it.
///
/// A window manager that reads the monitor set only as it starts tiles a maximized window
/// across the whole framebuffer rather than the per-display regions an extended layout
/// defines; restarted, it reads the set the layout published. None of this applies where
/// the displays are outputs of their own (`has_pluggable_outputs`): a second output arriving
/// is a hotplug, which every manager follows. Which managers need that is measured
/// (`RESTART_TO_READ_MONITORS`), not configured, and the one running is restarted with the
/// command line it was started with plus `--replace`, so it keeps the configuration chain
/// its session gave it, less the options that would run the session's autostart again
/// (`restart_command`). Both transports share this state: once per session, since a second
/// restart would take window management away from whoever is using it. Wayland sessions
/// manage their own windows and never restart.
#[derive(Default)]
pub struct MultiMonitorWindowManager {
    done: AtomicBool,
}

impl MultiMonitorWindowManager {
    /// A manager that has restarted nothing yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Restart the running window manager if it is one that needs it and this session has
    /// not yet.
    pub async fn ensure_for(&self, display_count: usize, is_wayland: bool) {
        if is_wayland || self.done.load(Ordering::Relaxed) || display_count <= 1 {
            return;
        }
        self.done.store(true, Ordering::Relaxed);
        if has_pluggable_outputs().await {
            info!(
                target: "display",
                "Multi-monitor setup: the displays are outputs of their own, which every window manager follows live; no restart."
            );
            return;
        }
        let _ = tokio::task::spawn_blocking(|| Self::restart_if_needed(&LiveWindowManagers)).await;
    }

    /// The decision against `host`, for a session that has two displays on a server
    /// without pluggable outputs. Blocking. Returns the command started, if any.
    pub fn restart_if_needed(host: &dyn WindowManagerHost) -> Option<Vec<String>> {
        let running = host.wm_name();
        let Some(needing) = RESTART_TO_READ_MONITORS
            .iter()
            .find(|wm| wm_name_matches(wm, &running))
        else {
            let name = if running.is_empty() {
                "no window manager".to_string()
            } else {
                running
            };
            info!(target: "display", "Multi-monitor setup: {name} keeps its monitor set current; no restart.");
            return None;
        };
        let replacing = host.wm_pid();
        let mut command = restart_command(&host.wm_command(replacing));
        if command.is_empty() {
            command.push(needing.to_string());
        }
        if !command.iter().any(|a| a == "--replace") {
            command.push("--replace".into());
        }
        info!(target: "display", "Multi-monitor setup: restarting {needing} to read the monitor set.");
        if let Err(e) = host.spawn(&command) {
            error!(target: "display", "Failed to restart {needing}: {e}");
            return None;
        }
        // Before the layout applies: a WM snapshotting the monitor set mid-swap re-tiles
        // maximized windows across the whole framebuffer.
        if !host.wait_for_wm(needing, replacing) {
            warn!(target: "display", "{needing} restart not confirmed; applying layout anyway.");
        }
        Some(command)
    }

    /// Run the decision against a stand-in host, for a session with `display_count`
    /// displays, honoring the once-per-session rule.
    pub fn ensure_for_with(
        &self,
        display_count: usize,
        is_wayland: bool,
        host: &dyn WindowManagerHost,
    ) -> Option<Vec<String>> {
        if is_wayland || self.done.load(Ordering::Relaxed) || display_count <= 1 {
            return None;
        }
        self.done.store(true, Ordering::Relaxed);
        Self::restart_if_needed(host)
    }
}

/// Name of the running EWMH window manager, empty when undetectable.
pub async fn current_wm_name() -> String {
    on_x11(|x| x.wm_name()).await.unwrap_or_default()
}

/// Process id the running EWMH window manager advertises, 0 when none.
pub async fn current_wm_pid() -> i32 {
    on_x11(|x| x.wm_pid()).await.unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A root grown ahead of the mode-set already has the layout's size while the output shows
    /// the primary alone, which still needs the mode-set; a CRTC covering the layout does not,
    /// even one that CVT alignment left wider, and a root of another size does as before.
    #[test]
    fn the_mode_set_follows_what_the_output_shows() {
        let info = |current: &str, shown: Option<&str>| ResolutionInfo {
            current: current.into(),
            fitted: current.into(),
            modes: Vec::new(),
            max: "8192x4096".into(),
            output: Some("screen".into()),
            shown: shown.map(str::to_string),
        };
        // The root, what the output shows, and whether a 4944x1612 layout needs a mode-set.
        let cases = [
            ("4944x1612", Some("3024x1612"), true),
            ("4944x1612", Some("4944x1612"), false),
            ("4944x1612", Some("4952x1612"), false),
            ("4944x1612", None, false),
            ("3024x1612", Some("3024x1612"), true),
            ("6864x1872", Some("6864x1872"), true),
        ];
        for (root, shown, want) in cases {
            assert_eq!(
                needs_mode_set(&info(root, shown), 4944, 1612),
                want,
                "{root} {shown:?}"
            );
        }
    }

    /// A manager advertising `name` from command line `cmdline`, recording what is started.
    struct Fake {
        name: String,
        cmdline: Vec<String>,
        started: RefCell<Vec<String>>,
    }

    impl Fake {
        fn new(name: &str, cmdline: &[&str]) -> Fake {
            Fake {
                name: name.into(),
                cmdline: cmdline.iter().map(|s| s.to_string()).collect(),
                started: RefCell::new(Vec::new()),
            }
        }
    }

    impl WindowManagerHost for Fake {
        fn wm_name(&self) -> String {
            self.name.clone()
        }

        fn wm_pid(&self) -> i32 {
            if self.cmdline.is_empty() { 0 } else { 4242 }
        }

        fn wm_command(&self, pid: i32) -> Vec<String> {
            if pid == 4242 {
                self.cmdline.clone()
            } else {
                Vec::new()
            }
        }

        fn spawn(&self, command: &[String]) -> std::io::Result<()> {
            self.started.borrow_mut().push(command.join(" "));
            Ok(())
        }

        fn wait_for_wm(&self, _name: &str, _replacing: i32) -> bool {
            true
        }
    }

    fn restart_attempt(
        name: &str,
        cmdline: &[&str],
        is_wayland: bool,
        displays: usize,
        manager: Option<&MultiMonitorWindowManager>,
    ) -> Vec<String> {
        let fake = Fake::new(name, cmdline);
        let fresh = MultiMonitorWindowManager::new();
        manager
            .unwrap_or(&fresh)
            .ensure_for_with(displays, is_wayland, &fake);
        fake.started.into_inner()
    }

    #[test]
    fn restart_decisions() {
        assert_eq!(
            restart_attempt(
                "Openbox",
                &["openbox", "--config-file", "/tmp/rc.xml"],
                false,
                2,
                None
            ),
            ["openbox --config-file /tmp/rc.xml --replace"]
        );
        assert_eq!(
            restart_attempt("Openbox", &["openbox", "--replace"], false, 2, None),
            ["openbox --replace"]
        );
        assert_eq!(
            restart_attempt("Xfwm4", &["xfwm4"], false, 2, None),
            ["xfwm4 --replace"]
        );
        assert_eq!(
            restart_attempt("Openbox", &[], false, 2, None),
            ["openbox --replace"]
        );
        let hook = [
            "openbox",
            "--startup",
            "/usr/lib/openbox-autostart OPENBOX",
            "--config-file",
            "/tmp/rc.xml",
        ];
        assert_eq!(
            restart_attempt("Openbox", &hook, false, 2, None),
            ["openbox --config-file /tmp/rc.xml --replace"]
        );
        assert_eq!(
            restart_attempt(
                "Openbox",
                &["openbox", "--sm-client-id", "10c"],
                false,
                2,
                None
            ),
            ["openbox --replace"]
        );
        assert!(restart_attempt("KWin", &["kwin_x11"], false, 2, None).is_empty());
        assert!(restart_attempt("Mutter", &["mutter"], false, 2, None).is_empty());
        assert!(restart_attempt("", &[], false, 2, None).is_empty());
        assert!(restart_attempt("Openbox", &["openbox"], false, 1, None).is_empty());
        assert!(restart_attempt("Openbox", &["openbox"], true, 2, None).is_empty());
        let manager = MultiMonitorWindowManager::new();
        restart_attempt("Openbox", &["openbox"], false, 2, Some(&manager));
        assert!(
            restart_attempt("Openbox", &["openbox"], false, 2, Some(&manager)).is_empty(),
            "once per session"
        );
    }

    #[test]
    fn restart_command_drops_session_options() {
        let cmd: Vec<String> = [
            "openbox",
            "--startup=/x",
            "--sm-client-id",
            "10c",
            "--config-file",
            "rc",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(restart_command(&cmd), ["openbox", "--config-file", "rc"]);
    }

    #[test]
    fn manager_names_match_their_commands() {
        let cases = [
            ("kwin_x11", "KWin", true),
            ("openbox", "Openbox", true),
            ("xfwm4", "Xfwm4", true),
            ("labwc", "labwc", true),
            ("kwin_x11", "Openbox", false),
            ("openbox", "", false),
            ("", "KWin", false),
            ("kwin_x11 --replace", "KWin", true),
        ];
        for (command, advertised, expected) in cases {
            assert_eq!(
                wm_name_matches(command, advertised),
                expected,
                "{command:?} vs {advertised:?}"
            );
        }
    }

    fn live(entries: &[(&str, Rect, bool, bool)]) -> BTreeMap<String, LiveMonitor> {
        entries
            .iter()
            .map(|(n, r, o, p)| {
                (
                    n.to_string(),
                    LiveMonitor {
                        rect: *r,
                        has_output: *o,
                        primary: *p,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn monitor_names_follow_rectangle_changes() {
        let layouts: Layouts = [
            ("primary".to_string(), Layout::new(0, 0, 1280, 720)),
            ("display2".to_string(), Layout::new(1280, 0, 800, 600)),
        ]
        .into_iter()
        .collect();
        let names = monitor_names(&layouts, &BTreeMap::new());
        assert_eq!(names["primary"], "selkies-primary");
        assert_eq!(names["display2"], "selkies-display2");
        let live_set = live(&[
            ("selkies-primary", (0, 0, 1280, 720), true, true),
            ("selkies-display2", (1280, 0, 1024, 768), true, false),
        ]);
        let names = monitor_names(&layouts, &live_set);
        assert_eq!(
            names["primary"], "selkies-primary",
            "an unchanged rectangle keeps its name"
        );
        assert_eq!(
            names["display2"], "selkies-display2@800x600+1280+0",
            "a resized display is named for its rectangle"
        );
        let alone: Layouts = [("primary".to_string(), Layout::new(0, 0, 1920, 1080))]
            .into_iter()
            .collect();
        assert_eq!(
            monitor_names(&alone, &live_set)["primary"],
            "selkies-primary",
            "the only display keeps its name"
        );
        assert_eq!(display_of("selkies-display2@800x600+1280+0"), "display2");
    }

    #[test]
    fn live_set_matching() {
        let desired: BTreeMap<String, Rect> = [
            ("selkies-primary".to_string(), (0, 0, 1280, 720)),
            ("selkies-display2".to_string(), (1280, 0, 800, 600)),
        ]
        .into_iter()
        .collect();
        let shared = live(&[
            ("selkies-primary", (0, 0, 1280, 720), true, true),
            ("selkies-display2", (1280, 0, 800, 600), true, false),
        ]);
        assert!(monitors_match(&shared, &desired, true, true));
        assert!(
            !monitors_match(&shared, &desired, false, true),
            "an output the server was measured to keep to itself is no match"
        );
        let primary_only = live(&[
            ("selkies-primary", (0, 0, 1280, 720), true, true),
            ("selkies-display2", (1280, 0, 800, 600), false, false),
        ]);
        assert!(monitors_match(&primary_only, &desired, false, true));
        let unflagged = live(&[
            ("selkies-primary", (0, 0, 1280, 720), true, false),
            ("selkies-display2", (1280, 0, 800, 600), true, false),
        ]);
        assert!(
            !monitors_match(&unflagged, &desired, true, true),
            "the primary flag has to be on the primary"
        );
        let outputless = live(&[
            ("selkies-primary", (0, 0, 1280, 720), false, true),
            ("selkies-display2", (1280, 0, 800, 600), false, false),
        ]);
        assert!(monitors_match(&outputless, &desired, true, false));
        assert!(
            !monitors_match(&shared, &desired, true, false),
            "a stale set listing an output the server lacks is no match"
        );
    }

    /// Start a private Xvfb on a display nothing holds; `None` where none can.
    fn private_x_server() -> Option<(std::process::Child, String)> {
        if !super::super::xrandr::which("Xvfb") {
            return None;
        }
        for number in 64..256 {
            if std::path::Path::new(&format!("/tmp/.X{number}-lock")).exists()
                || std::path::Path::new(&format!("/tmp/.X11-unix/X{number}")).exists()
            {
                continue;
            }
            let display = format!(":{number}");
            let Ok(mut child) = std::process::Command::new("Xvfb")
                .args([
                    &display,
                    "-screen",
                    "0",
                    "640x480x24",
                    "-extension",
                    "GLX",
                    "-nolisten",
                    "tcp",
                    "-ac",
                    "-noreset",
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            else {
                return None;
            };
            let deadline = Instant::now() + Duration::from_secs(15);
            while Instant::now() < deadline {
                if child.try_wait().ok().flatten().is_some() {
                    break;
                }
                if std::path::Path::new(&format!("/tmp/.X11-unix/X{number}")).exists() {
                    return Some((child, display));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        None
    }

    /// The readers against a real Openbox on an X server of the test's own, and a real
    /// restart of it, which carries no startup hook a second time.
    #[test]
    #[ignore = "starts a private Xvfb and Openbox; sets DISPLAY for the process"]
    fn live_openbox_is_read_and_restarted() {
        if !super::super::xrandr::which("openbox") {
            eprintln!("no openbox on PATH; skipping");
            return;
        }
        let Some((mut xvfb, display)) = private_x_server() else {
            eprintln!("no private X server could be started; skipping");
            return;
        };
        // The readers open the display the environment names; never the desktop's. The
        // DISPLAY the process had comes back at the end, for the live tests run after this.
        let previous = std::env::var_os("DISPLAY");
        // SAFETY: the test binary is single-threaded for this ignored test, run alone.
        unsafe { std::env::set_var("DISPLAY", &display) };
        x11::drop_connection();
        let scratch = tempfile::Builder::new()
            .prefix("selkies-wm-autostart-")
            .tempdir()
            .expect("scratch");
        let marker = scratch.path().join("ran");
        let script = scratch.path().join("autostart.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\necho ran >> {}\n", marker.display()),
        )
        .expect("script");
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("chmod");
        let _ = std::fs::remove_file(&marker);
        let runs = || {
            std::fs::read_to_string(&marker)
                .map(|t| t.matches("ran").count())
                .unwrap_or(0)
        };
        let host = LiveWindowManagers;
        let mut openbox = std::process::Command::new("openbox")
            .args(["--replace", "--startup", script.to_str().expect("path")])
            .env("DISPLAY", &display)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("openbox");
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline && !(host.wm_pid() == openbox.id() as i32 && runs() == 1) {
            std::thread::sleep(Duration::from_millis(200));
        }
        let outcome = std::panic::catch_unwind(|| {
            let host = LiveWindowManagers;
            assert_eq!(
                host.wm_pid(),
                openbox.id() as i32,
                "the running Openbox advertises its process id"
            );
            assert!(
                wm_name_matches("openbox", &host.wm_name()),
                "and its name: {:?}",
                host.wm_name()
            );
            assert_eq!(runs(), 1, "the session's autostart runs as Openbox starts");
            let cmd = wm_command(openbox.id() as i32);
            assert_eq!(cmd[0], "openbox");
            assert!(cmd.contains(&"--startup".to_string()), "{cmd:?}");
            let started = MultiMonitorWindowManager::restart_if_needed(&host).expect("a restart");
            assert!(
                !started.contains(&"--startup".to_string()),
                "the replacement carries no startup hook: {started:?}"
            );
            let on_return = host.wm_pid();
            assert!(
                on_return != 0 && on_return != openbox.id() as i32,
                "returns once the replacement manages: {on_return}"
            );
            std::thread::sleep(Duration::from_millis(1500));
            assert_eq!(runs(), 1, "the restart leaves the autostart at one run");
            let new_cmd = wm_command(on_return);
            assert!(!new_cmd.contains(&"--startup".to_string()), "{new_cmd:?}");
            on_return
        });
        if let Ok(new_pid) = outcome
            && new_pid > 0
        {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(new_pid),
                nix::sys::signal::Signal::SIGTERM,
            );
        }
        let _ = openbox.kill();
        let _ = openbox.wait();
        x11::drop_connection();
        let _ = xvfb.kill();
        let _ = xvfb.wait();
        // SAFETY: as above.
        unsafe {
            match previous {
                Some(value) => std::env::set_var("DISPLAY", value),
                None => std::env::remove_var("DISPLAY"),
            }
        }
        if let Err(e) = outcome {
            std::panic::resume_unwind(e);
        }
    }
}
