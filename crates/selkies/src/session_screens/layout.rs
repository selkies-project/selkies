/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The Wayland layout passes over both compositors, in the one order they allow.
//!
//! The capture compositor refuses a placement that overlaps a live output, and the primary's
//! screen takes a new size only once its capture has restarted, so a layout removes, shrinks, and
//! moves before it grows. A secondary whose rectangle shrinks at its origin (a page whose density
//! changed, a smaller browser window) leaves the primary's new offset inside its old rectangle, so
//! it gives that room up before the primary moves, and the capture start that follows grows it
//! into the room the move left. A secondary that moves is a destroy and recreate. The session
//! compositor keeps one screen per laid-out display: the ones whose displays left are retired
//! with their capture outputs, and each new display's is grown right before the output that adopts
//! its host window, then given the display's own DPI, what the session leaves being that output's
//! capture scale. A recreated output takes back the window it lost, since parked windows carry the
//! output they came from, and each fresh screen is grown right before its own output, so the
//! newest-parked fallback claims the window just added, never a waiting leftover.
//!
//! WebSockets applies a whole layout around the primary's capture start ([`before_primary`],
//! [`after_primary`]); WebRTC places one secondary ahead of its pipeline ([`apply_extension`],
//! [`retire_outputs`]).

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use log::{error, info, warn};

use super::SessionScreens;
use crate::core::PRIMARY_DISPLAY;
use crate::display::wayland::{SecondaryAction, secondary_action};
use crate::display::{Layouts, WAYLAND_SCREEN_OUTPUT_ID, WaylandOutputs, wayland_output_id};
use crate::ws::host::{DroppedDisplay, LayoutOutcome};

/// What a display whose screen is refused is told.
const SCREEN_REFUSED: &str = "The session compositor cannot add a screen for this display.";
/// What a display whose output is refused is told.
const OUTPUT_REFUSED: &str = "The compositor cannot create an output for this display.";

/// The scale a DPI asks of a capture output where no session takes it.
fn dpi_scale(dpi: &str) -> f64 {
    dpi.trim().parse::<f64>().map_or(1.0, |dpi| dpi / 96.0)
}

/// The primary's capture, for a secondary dropping into room a shrinking primary frees.
#[async_trait]
pub trait PrimaryCapture: Send + Sync {
    /// Have the live capture take this rectangle, if it holds another, and tell the pages what
    /// the compositor realized.
    async fn fit(&self, x: i32, y: i32, width: i32, height: i32);
}

/// The first half of a whole-layout apply, before the primary's capture start: destroy what no
/// display wants, shrink or recreate the secondaries, move the primary (recreating what blocks it,
/// and re-anchoring at the origin without its secondaries where nothing frees it), grow its screen
/// to hold its rectangle at `primary_scale`, and retire the session screens of displays that left.
/// `keep` names the live captures that follow the layout; those whose output went away are in the
/// outcome's `restart`.
pub async fn before_primary(
    outputs: &WaylandOutputs,
    screens: Option<&Arc<SessionScreens>>,
    layouts: &mut Layouts,
    keep: &HashSet<String>,
    primary_scale: f64,
) -> LayoutOutcome {
    let mut outcome = LayoutOutcome::default();
    let existing = outputs.list().await;
    let mut wanted: Vec<(u32, String)> = layouts
        .keys()
        .filter(|d| *d != PRIMARY_DISPLAY)
        .map(|d| (wayland_output_id(Some(d)), d.clone()))
        .collect();
    wanted.sort();
    // The primary's screen and the view its capture binds to always persist.
    let session = [WAYLAND_SCREEN_OUTPUT_ID, wayland_output_id(None)];
    for desc in existing.values() {
        let oid = desc.0;
        if !session.contains(&oid) && !wanted.iter().any(|(id, _)| *id == oid) {
            info!(target: "display", "Destroying stale Wayland output {oid}.");
            outputs.destroy_output(oid).await;
        }
    }
    let mut present: HashSet<u32> = existing.keys().copied().collect();
    for (oid, did) in &wanted {
        let (Some(layout), Some(desc)) = (layouts.get(did).copied(), existing.get(oid)) else {
            continue;
        };
        let recreate = match secondary_action(desc, &layout) {
            SecondaryAction::Keep => None,
            SecondaryAction::Recreate(why) => Some(why),
            SecondaryAction::Shrink => (!outputs.shrink_output(desc, layout.w, layout.h).await)
                .then(|| format!("cannot shrink to {}x{}", layout.w, layout.h)),
        };
        if let Some(why) = recreate {
            info!(target: "display", "Wayland output {oid} {why}; recreating it.");
            outputs.destroy_output(*oid).await;
            present.remove(oid);
            if keep.contains(did) {
                outcome.restart.push(did.clone());
            }
        }
    }
    let target = layouts
        .get(PRIMARY_DISPLAY)
        .map(|p| (p.x, p.y))
        .unwrap_or((0, 0));
    let current = existing
        .get(&WAYLAND_SCREEN_OUTPUT_ID)
        .map(|d| (d.1, d.2))
        .unwrap_or((0, 0));
    if target != current {
        let mut moved = outputs.reposition_primary(target.0, target.1).await;
        if !moved && wanted.iter().any(|(oid, _)| present.contains(oid)) {
            for (oid, did) in &wanted {
                if present.remove(oid) {
                    info!(target: "display", "Wayland output {oid} blocks the primary's move; recreating it.");
                    outputs.destroy_output(*oid).await;
                    if keep.contains(did) {
                        outcome.restart.push(did.clone());
                    }
                }
            }
            moved = outputs.reposition_primary(target.0, target.1).await;
        }
        if !moved {
            outputs.reposition_primary(0, 0).await;
            if let Some(primary) = layouts.get_mut(PRIMARY_DISPLAY) {
                primary.x = 0;
                primary.y = 0;
            }
            outcome.reanchored = true;
            for (_, did) in &wanted {
                layouts.remove(did);
                outcome.dropped.push(DroppedDisplay {
                    display_id: did.clone(),
                    reason: "The compositor cannot move the primary output for this arrangement."
                        .into(),
                });
            }
        }
    }
    if let Some(primary) = layouts.get(PRIMARY_DISPLAY) {
        outputs
            .size_screen(primary.w, primary.h, primary_scale, true)
            .await;
    }
    if let Some(screens) = screens {
        let attached: Vec<String> = layouts
            .keys()
            .filter(|d| *d != PRIMARY_DISPLAY)
            .cloned()
            .collect();
        screens.ensure_screens(&attached).await;
        screens.resync();
    }
    outcome
}

/// The second half, once the primary's capture start sized its screen: give every laid-out
/// secondary its session screen and its capture output, at its rectangle and at the scale the
/// session left for it, by `dpis` (display to DPI). A display either compositor refuses is dropped
/// from `layouts` with that reason, and the primary returns to the origin where the arrangement
/// was built around it, which its capture follows live. The scale of each output created is in
/// the outcome's `scales`.
pub async fn after_primary(
    outputs: &WaylandOutputs,
    screens: Option<&Arc<SessionScreens>>,
    layouts: &mut Layouts,
    dpis: &HashMap<String, String>,
) -> LayoutOutcome {
    let mut outcome = LayoutOutcome::default();
    let mut wanted: HashMap<u32, String> = HashMap::new();
    let secondaries: Vec<String> = layouts
        .keys()
        .filter(|d| *d != PRIMARY_DISPLAY)
        .cloned()
        .collect();
    for did in &secondaries {
        match wanted.entry(wayland_output_id(Some(did))) {
            Entry::Occupied(_) => {
                layouts.remove(did);
                outcome.dropped.push(DroppedDisplay {
                    display_id: did.clone(),
                    reason: "This display's name collides with another display's output.".into(),
                });
            }
            Entry::Vacant(free) => {
                free.insert(did.clone());
            }
        }
    }
    if wanted.is_empty() {
        return outcome;
    }
    let existing = outputs.list().await;
    let mut ordered: Vec<(u32, String)> = wanted.into_iter().collect();
    ordered.sort();
    let mut created_any = false;
    for (oid, did) in ordered {
        if existing.contains_key(&oid) {
            continue;
        }
        let Some(layout) = layouts.get(&did).copied() else {
            continue;
        };
        let dpi = dpis.get(&did).map_or("96", String::as_str);
        let mut scale = dpi_scale(dpi);
        let mut refusal = None;
        if let Some(screens) = screens {
            if screens
                .ensure_screen(&did, Some((layout.w, layout.h)), scale)
                .await
            {
                scale = screens
                    .realize_dpi(dpi, &did, Some((layout.w, layout.h)))
                    .await;
            } else {
                refusal = Some(SCREEN_REFUSED);
            }
        }
        if refusal.is_none() {
            if outputs
                .create_output(&did, layout.x, layout.y, layout.w, layout.h, scale)
                .await
            {
                outcome.scales.insert(did.clone(), scale);
                created_any = true;
                continue;
            }
            error!(target: "display", "Wayland create_output {oid} failed.");
            refusal = Some(OUTPUT_REFUSED);
        }
        layouts.remove(&did);
        outcome.dropped.push(DroppedDisplay {
            display_id: did,
            reason: refusal.unwrap_or(OUTPUT_REFUSED).into(),
        });
        if let Some(primary) = layouts.get_mut(PRIMARY_DISPLAY)
            && (primary.x, primary.y) != (0, 0)
            && outputs.reposition_primary(0, 0).await
        {
            primary.x = 0;
            primary.y = 0;
        }
    }
    if created_any && let Some(screens) = screens {
        screens.resync();
    }
    outcome
}

/// Retire every secondary's capture output and session screen except `keep`'s: the kept
/// secondary's while it is being placed, only the primary's on teardown, where a removed screen's
/// windows return to the primary. The primary's screen and its view always persist.
pub async fn retire_outputs(
    outputs: &WaylandOutputs,
    screens: Option<&Arc<SessionScreens>>,
    keep: Option<&str>,
) {
    let kept: Vec<&str> = keep.into_iter().collect();
    outputs.destroy_secondary_outputs(&kept).await;
    if let Some(screens) = screens {
        let attached: Vec<String> = kept.iter().map(|d| d.to_string()).collect();
        screens.ensure_screens(&attached).await;
        screens.resync();
    }
}

/// Realize the extended layout for `did` as compositor screens, before its pipeline binds a
/// capture: the Wayland counterpart of `apply_extended_layout`, for a transport that places a
/// secondary at a time. The primary moves to its layout offset; a secondary that moves is
/// destroyed before it, so the rectangles never overlap; one that keeps its origin but shrinks
/// gives the room up in place ahead of the move and grows into the room the move leaves on the
/// restart that follows; one the compositor still finds in the primary's way is recreated. A
/// secondary dropping into the room a shrinking primary frees waits for the primary's capture to
/// take the new rectangle and its screen to give the room up, since the compositor refuses both
/// an output overlapping the primary's rectangle and a screen too small for the live view.
///
/// Returns `Err` with why the display's screen is not in place, which the caller drops the display
/// with: the session compositor refused it a screen, or the capture compositor cannot create its
/// output or move the primary.
pub async fn apply_extension(
    outputs: &WaylandOutputs,
    screens: Option<&Arc<SessionScreens>>,
    did: &str,
    layouts: &Layouts,
    dpi: &str,
    primary: &dyn PrimaryCapture,
) -> Result<(), String> {
    let Some(rect) = layouts.get(did).copied() else {
        return Err(OUTPUT_REFUSED.into());
    };
    let oid = wayland_output_id(Some(did));
    let listed = outputs.list().await;
    retire_outputs(outputs, screens, Some(did)).await;
    let mut existing = listed.get(&oid).copied();
    if let Some(desc) = existing {
        if (desc.1, desc.2) != (rect.x, rect.y) {
            info!(target: "display", "Wayland output {oid} moves to +{}+{}; recreating it.", rect.x, rect.y);
            outputs.destroy_output(oid).await;
            existing = None;
        } else if !outputs.shrink_output(&desc, rect.w, rect.h).await {
            info!(target: "display", "Wayland output {oid} cannot shrink to {}x{}; recreating it.", rect.w, rect.h);
            outputs.destroy_output(oid).await;
            existing = None;
        }
    }
    let target = layouts.get(PRIMARY_DISPLAY).copied();
    let origin = target.map_or((0, 0), |p| (p.x, p.y));
    let screen = listed.get(&WAYLAND_SCREEN_OUTPUT_ID).copied();
    if origin != screen.map_or((0, 0), |s| (s.1, s.2)) {
        let mut moved = outputs.reposition_primary(origin.0, origin.1).await;
        if !moved && existing.is_some() {
            info!(target: "display", "Wayland output {oid} blocks the primary's move; recreating it.");
            outputs.destroy_output(oid).await;
            existing = None;
            moved = outputs.reposition_primary(origin.0, origin.1).await;
        }
        if !moved {
            return Err(OUTPUT_REFUSED.into());
        }
    }
    if existing.is_some() {
        return Ok(());
    }
    if let (Some(p), Some(screen)) = (target, screen)
        && p.w > 0
        && p.h > 0
        && (p.w < screen.3 || p.h < screen.4)
    {
        primary.fit(p.x, p.y, p.w, p.h).await;
        if !outputs.size_screen(p.w, p.h, screen.5, false).await {
            warn!(target: "display", "Wayland screen shrink to {}x{} refused.", p.w, p.h);
        }
    }
    let mut scale = dpi_scale(dpi);
    if let Some(screens) = screens {
        if !screens
            .ensure_screen(did, Some((rect.w, rect.h)), scale)
            .await
        {
            return Err(SCREEN_REFUSED.into());
        }
        scale = screens.realize_dpi(dpi, did, Some((rect.w, rect.h))).await;
    }
    if !outputs
        .create_output(did, rect.x, rect.y, rect.w, rect.h, scale)
        .await
    {
        return Err(OUTPUT_REFUSED.into());
    }
    if let Some(screens) = screens {
        screens.resync();
    }
    Ok(())
}
