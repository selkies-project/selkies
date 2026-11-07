/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The Wayland side of display management: the compositor output each display addresses
//! and the output operations both transports run through `pixelflux::Compositor`.
//!
//! Every display is a screen of the session compositor's own: the primary shows output 0,
//! the screen the session boots on, which is never destroyed, and its capture binds to the
//! view (output 1) covering it; each secondary owns an output created beside it
//! (`wayland_output_id`), so the session has as many monitors as the client is shown
//! displays and lays its windows and panels out per monitor. The compositor refuses any
//! placement that overlaps a live output, so a relayout removes, shrinks, and moves before
//! it grows (`shrink_output`, `reposition_primary`, `size_screen`), and a secondary that
//! moves is destroyed and recreated by the capture start that follows. A DPI realizes here
//! as the output's scale, never as Xft resources (`dpi`), and the cursor theme is resized on
//! the compositor (`set_cursor_size`).
//!
//! Every call blocks on the compositor thread's answer for up to two seconds, so each runs
//! on a blocking thread of the runtime.

use std::collections::BTreeMap;
use std::sync::Arc;

use log::{error, warn};
use pixelflux::{Compositor, Error, OutputDesc};

use crate::core::PRIMARY_DISPLAY;

/// The compositor screen the primary display shows: the output the session compositor
/// boots on, which is never destroyed. Its size is the primary's, and every secondary
/// display is a screen of its own beside it.
pub const WAYLAND_SCREEN_OUTPUT_ID: u32 = 0;

/// Stable compositor id for a display name, shared by both transports.
///
/// `primary` maps to 1, the view covering screen 0 that its capture binds to, and
/// `displayN` to N, the screen that display owns. A secondary name without a numeric suffix
/// falls back to 2, and so does one whose digits would land on the primary's screen or view
/// (`display0`, `display1`): a client-chosen name must never address the session's own
/// nodes.
pub fn wayland_output_id(display_id: Option<&str>) -> u32 {
    let Some(name) = display_id.filter(|d| !d.is_empty() && *d != PRIMARY_DISPLAY) else {
        return 1;
    };
    let stem = name.trim_end_matches(|c: char| c.is_ascii_digit());
    let digits = &name[stem.len()..];
    let n = if (1..=9).contains(&digits.len()) {
        digits.parse::<u32>().unwrap_or(2)
    } else {
        2
    };
    n.max(2)
}

/// The compositor's outputs by id.
pub type Outputs = BTreeMap<u32, OutputDesc>;

/// The capture compositor's output calls, one to one, so the layout passes and their shrink and
/// fit rules run against a model of its overlap rule in the suites and `Compositor` in a session.
pub trait CompositorOutputs: Send + Sync + 'static {
    /// Every live output.
    fn list_outputs(&self) -> Result<Vec<OutputDesc>, Error>;
    /// Create an output; false when the id is taken or the rectangle overlaps a live output.
    fn create_output(
        &self,
        id: u32,
        width: i32,
        height: i32,
        x: i32,
        y: i32,
        scale: f64,
    ) -> Result<bool, Error>;
    /// Destroy an output; false for the primary or an unknown id.
    fn destroy_output(&self, id: u32) -> Result<bool, Error>;
    /// Move an output; false when the rectangle overlaps a live output.
    fn reposition_output(&self, id: u32, x: i32, y: i32) -> Result<bool, Error>;
    /// Resize an output in place.
    fn resize_output(&self, id: u32, width: i32, height: i32, scale: f64) -> Result<bool, Error>;
    /// Recreate the cursor theme at `size` pixels.
    fn set_cursor_size(&self, size: i32) -> Result<bool, Error>;
    /// The realized size and scale of an output, past the queued capture start.
    fn get_realized_geometry(&self, id: u32) -> Result<Option<(i32, i32, f64)>, Error>;
}

impl CompositorOutputs for Compositor {
    fn list_outputs(&self) -> Result<Vec<OutputDesc>, Error> {
        Compositor::list_outputs(self)
    }

    fn create_output(
        &self,
        id: u32,
        width: i32,
        height: i32,
        x: i32,
        y: i32,
        scale: f64,
    ) -> Result<bool, Error> {
        Compositor::create_output(self, id, width, height, x, y, scale)
    }

    fn destroy_output(&self, id: u32) -> Result<bool, Error> {
        Compositor::destroy_output(self, id)
    }

    fn reposition_output(&self, id: u32, x: i32, y: i32) -> Result<bool, Error> {
        Compositor::reposition_output(self, id, x, y)
    }

    fn resize_output(&self, id: u32, width: i32, height: i32, scale: f64) -> Result<bool, Error> {
        Compositor::resize_output(self, id, width, height, scale)
    }

    fn set_cursor_size(&self, size: i32) -> Result<bool, Error> {
        Compositor::set_cursor_size(self, size)
    }

    fn get_realized_geometry(&self, id: u32) -> Result<Option<(i32, i32, f64)>, Error> {
        Compositor::get_realized_geometry(self, id)
    }
}

/// The compositor output operations, each off the runtime's workers.
#[derive(Clone)]
pub struct WaylandOutputs {
    compositor: Arc<dyn CompositorOutputs>,
}

impl WaylandOutputs {
    /// Output management over `compositor`.
    pub fn new(compositor: Arc<dyn CompositorOutputs>) -> WaylandOutputs {
        WaylandOutputs { compositor }
    }

    /// Output management over the running compositor, `None` before one runs.
    pub fn running() -> Option<WaylandOutputs> {
        pixelflux::running_compositor().map(|c| WaylandOutputs::new(c))
    }

    async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(&dyn CompositorOutputs) -> T + Send + 'static,
    ) -> Option<T> {
        let compositor = self.compositor.clone();
        tokio::task::spawn_blocking(move || f(&*compositor))
            .await
            .ok()
    }

    /// Every live output by id; empty when the compositor cannot be asked.
    pub async fn list(&self) -> Outputs {
        match self.blocking(|c| c.list_outputs()).await {
            Some(Ok(outputs)) => outputs.into_iter().map(|o| (o.0, o)).collect(),
            Some(Err(e)) => {
                error!(target: "display", "Wayland list_outputs failed: {e}");
                Outputs::new()
            }
            None => Outputs::new(),
        }
    }

    /// Move the primary screen (output 0) to a union-layout offset: the Wayland counterpart
    /// of laying the primary at a non-origin RandR position for `left`/`up` arrangements,
    /// and of re-anchoring it at the origin on teardown. The compositor remaps the screen,
    /// the view its capture binds to, its windows, and the input offset live; the capture
    /// follows without a restart. False when the compositor refuses the move.
    pub async fn reposition_primary(&self, x: i32, y: i32) -> bool {
        match self
            .blocking(move |c| c.reposition_output(WAYLAND_SCREEN_OUTPUT_ID, x, y))
            .await
        {
            Some(Ok(moved)) => moved,
            Some(Err(e)) => {
                error!(target: "display", "Wayland primary reposition to +{x}+{y} failed: {e}");
                false
            }
            None => false,
        }
    }

    /// Shrink a secondary output to no more than `width` by `height` on each axis.
    ///
    /// The layout pass moves the primary before the captures restart, and the compositor
    /// refuses a move into room a live output still holds: a secondary whose rectangle
    /// shrinks (a page changing density, a smaller browser window) has to give that room up
    /// first. Only the shrink happens here, at the scale the output holds; the capture start
    /// that follows the primary's move grows the output to its whole rectangle and scale,
    /// into room the move has left. Returns whether the output now holds no more than the
    /// asked rectangle; false when the compositor refused, for the caller to recreate the
    /// output instead.
    pub async fn shrink_output(&self, output: &OutputDesc, width: i32, height: i32) -> bool {
        let (oid, _, _, cur_w, cur_h, scale, _) = *output;
        let (fit_w, fit_h) = (cur_w.min(width), cur_h.min(height));
        if (fit_w, fit_h) == (cur_w, cur_h) {
            return true;
        }
        match self
            .blocking(move |c| c.resize_output(oid, fit_w, fit_h, scale))
            .await
        {
            Some(Ok(true)) => true,
            Some(Ok(false)) => {
                warn!(target: "display", "Wayland output {oid} shrink to {fit_w}x{fit_h} refused.");
                false
            }
            Some(Err(e)) => {
                error!(target: "display", "Wayland output {oid} shrink to {fit_w}x{fit_h} failed: {e}");
                false
            }
            None => false,
        }
    }

    /// Size the primary's screen (output 0) to its display rectangle at `scale`.
    ///
    /// The primary's capture binds to the view covering that screen, and a capture start
    /// sizes the view alone, which the screen must already hold: so the screen is grown
    /// ahead of the capture restart (`grow_only`) and fitted to the rectangle once the
    /// capture carries it -- an early shrink is refused by the compositor, which leaves no
    /// view hanging outside its screen.
    pub async fn size_screen(&self, width: i32, height: i32, scale: f64, grow_only: bool) -> bool {
        if width <= 0 || height <= 0 {
            return false;
        }
        let (mut width, mut height) = (width, height);
        if grow_only && let Some(screen) = self.list().await.get(&WAYLAND_SCREEN_OUTPUT_ID) {
            width = width.max(screen.3);
            height = height.max(screen.4);
        }
        match self
            .blocking(move |c| c.resize_output(WAYLAND_SCREEN_OUTPUT_ID, width, height, scale))
            .await
        {
            Some(Ok(true)) => true,
            Some(Ok(false)) => {
                warn!(target: "display", "Wayland screen resize to {width}x{height} refused.");
                false
            }
            Some(Err(e)) => {
                error!(target: "display", "Wayland resize_output failed: {e}");
                false
            }
            None => false,
        }
    }

    /// Give the primary's screen the scale `scale` at the size it has: a session that takes
    /// its scale from the host window's preferred fractional scale (a nested KWin) sees only
    /// the screen's. A no-op for a compositor without a screen yet.
    pub async fn scale_screen(&self, scale: f64) -> bool {
        match self.list().await.get(&WAYLAND_SCREEN_OUTPUT_ID) {
            Some(screen) => self.size_screen(screen.3, screen.4, scale, false).await,
            None => false,
        }
    }

    /// Create a secondary display's output at its layout rectangle and scale. False when
    /// the compositor refuses (the id is taken, the rectangle overlaps a live output, or no
    /// render target could be allocated).
    pub async fn create_output(
        &self,
        display_id: &str,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        scale: f64,
    ) -> bool {
        let oid = wayland_output_id(Some(display_id));
        match self
            .blocking(move |c| c.create_output(oid, width, height, x, y, scale))
            .await
        {
            Some(Ok(created)) => created,
            Some(Err(e)) => {
                error!(target: "display", "Wayland create_output {oid} failed: {e}");
                false
            }
            None => false,
        }
    }

    /// Destroy output `oid`; false for the primary or an unknown id.
    pub async fn destroy_output(&self, oid: u32) -> bool {
        match self.blocking(move |c| c.destroy_output(oid)).await {
            Some(Ok(destroyed)) => destroyed,
            Some(Err(e)) => {
                error!(target: "display", "Wayland destroy_output {oid} failed: {e}");
                false
            }
            None => false,
        }
    }

    /// Retire every secondary display's compositor screen except those of `keep`. The
    /// primary's screen (output 0) and the view its capture binds to always persist: the
    /// primary is the session, not an extension of it.
    pub async fn destroy_secondary_outputs(&self, keep: &[&str]) -> Vec<u32> {
        let mut kept: Vec<u32> = vec![WAYLAND_SCREEN_OUTPUT_ID, wayland_output_id(None)];
        kept.extend(keep.iter().map(|d| wayland_output_id(Some(d))));
        let mut destroyed = Vec::new();
        for oid in self.list().await.keys().copied() {
            if !kept.contains(&oid) && self.destroy_output(oid).await {
                destroyed.push(oid);
            }
        }
        destroyed
    }

    /// Recreate the compositor's cursor theme at `size` pixels, live: the Wayland
    /// counterpart of the X11 per-DPI cursor resize, with no capture restart.
    pub async fn set_cursor_size(&self, size: i32) -> bool {
        match self.blocking(move |c| c.set_cursor_size(size)).await {
            Some(Ok(true)) => true,
            Some(Ok(false)) => {
                warn!(target: "display", "Wayland compositor refused cursor size {size}.");
                false
            }
            Some(Err(e)) => {
                warn!(target: "display", "Wayland cursor resize failed: {e}");
                false
            }
            None => false,
        }
    }

    /// The size and scale the compositor realized for `display_id`'s output, `None` when
    /// it cannot be read or the read timed out (unknown geometry, not nothing to reconcile).
    pub async fn realized_geometry(&self, display_id: &str) -> Option<(i32, i32, f64)> {
        let oid = wayland_output_id(Some(display_id));
        match self.blocking(move |c| c.get_realized_geometry(oid)).await {
            Some(Ok(Some(geom))) if geom.0 > 0 && geom.1 > 0 => Some(geom),
            Some(Ok(_)) => None,
            Some(Err(e)) => {
                warn!(target: "display", "Wayland realized-geometry read failed for '{display_id}': {e}");
                None
            }
            None => None,
        }
    }
}

/// Why a secondary output in place is not kept as it is for a new layout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SecondaryAction {
    /// The output sits at its layout origin and fits: keep it.
    Keep,
    /// The output moves: a reposition is a destroy and recreate.
    Recreate(String),
    /// The output stays put but has to give room up before the primary moves.
    Shrink,
}

/// What a relayout does with a secondary's output already in the compositor.
pub fn secondary_action(existing: &OutputDesc, layout: &super::Layout) -> SecondaryAction {
    if (existing.1, existing.2) != (layout.x, layout.y) {
        return SecondaryAction::Recreate(format!("moves to +{}+{}", layout.x, layout.y));
    }
    if existing.3 > layout.w || existing.4 > layout.h {
        return SecondaryAction::Shrink;
    }
    SecondaryAction::Keep
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_ids_never_address_the_session_nodes() {
        assert_eq!(wayland_output_id(None), 1);
        assert_eq!(wayland_output_id(Some("")), 1);
        assert_eq!(wayland_output_id(Some("primary")), 1);
        assert_eq!(wayland_output_id(Some("display2")), 2);
        assert_eq!(wayland_output_id(Some("display7")), 7);
        assert_eq!(wayland_output_id(Some("display0")), 2);
        assert_eq!(wayland_output_id(Some("display1")), 2);
        assert_eq!(wayland_output_id(Some("secondary")), 2);
        assert_eq!(
            wayland_output_id(Some("d1234567890")),
            2,
            "ten digits are no suffix"
        );
    }

    #[test]
    fn secondary_outputs_keep_shrink_or_recreate() {
        let layout = super::super::Layout::new(1280, 0, 800, 600);
        assert_eq!(
            secondary_action(&(2, 1280, 0, 800, 600, 1.0, true), &layout),
            SecondaryAction::Keep
        );
        assert_eq!(
            secondary_action(&(2, 1280, 0, 600, 400, 1.0, true), &layout),
            SecondaryAction::Keep
        );
        assert_eq!(
            secondary_action(&(2, 1280, 0, 1024, 768, 1.0, true), &layout),
            SecondaryAction::Shrink
        );
        assert!(matches!(
            secondary_action(&(2, 0, 0, 800, 600, 1.0, true), &layout),
            SecondaryAction::Recreate(_)
        ));
    }
}
