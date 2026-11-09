/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Resizing and the extended desktop: a lone primary resizes the real display, and a secondary's
//! arrival turns every resolution into a layout pass.
//!
//! X11 captures of a pinned region move to their new rectangle before the root shrinks under them
//! and after it grows (`retarget_ahead_of_root`): a grab past the root's edge fails, and NvFBC cannot
//! rebuild a session whose box the root no longer holds. On Wayland the layout is compositor outputs
//! instead, whose start sizes the screen, so the screen is grown ahead of the capture restart and
//! fitted to what the compositor realized after it.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Weak};

use log::{debug, error, info, warn};

use crate::display::geometry::{
    align_dims_16, clamp_primary_feedback, compute_dual_layout, parse_resize_dims,
};
use crate::display::{Layout, Layouts, wayland_output_id};
use crate::settings::settings;
use crate::webrtc::pipeline::{MediaPipeline, PipelineHost};
use crate::webrtc::service::WebRtcService;

/// A display's pinned capture and the rectangle it moves to.
type CaptureMove = (String, Arc<dyn MediaPipeline>, (i32, i32, i32, i32));

impl WebRtcService {
    /// Route a client resolution to its display: the primary resizes the real display directly
    /// while it is alone; once a secondary is connected, or for any secondary, the resolution feeds
    /// the extended-desktop layout instead. The layout path honors an admin's manual-resolution
    /// lock the way the single-display path does: a second screen cannot be the way a client
    /// escapes it.
    pub async fn on_resize_handler(&self, res: &str, display: &str) {
        debug!(target: "webrtc", "Resize message for display '{display}': {res}");
        if display == "primary" && !self.policy.lock().enable_resize {
            warn!(target: "webrtc", "remote resizing disabled, skipping resize to {res}");
            return;
        }
        let has_secondaries = self.st.lock().displays.keys().any(|d| d != "primary");
        if display != "primary" || has_secondaries {
            let (w, h) = match self.server_locked_dims() {
                Some(dims) => {
                    warn!(target: "webrtc", "Client attempted to resize to {res} but server is in manual resolution mode. Using the configured {}x{} instead.", dims.0, dims.1);
                    dims
                }
                None => {
                    let Some((mut w, mut h)) = parse_resize_dims(res) else {
                        error!(target: "webrtc", "Invalid resize request: {res}");
                        return;
                    };
                    let aligned = self
                        .st
                        .lock()
                        .displays
                        .get(display)
                        .is_some_and(|d| d.force_aligned_resolution);
                    if aligned {
                        (w, h) = align_dims_16(w, h);
                    }
                    (w, h)
                }
            };
            {
                let mut st = self.st.lock();
                if display == "primary" {
                    st.primary_dims = Some((w, h));
                } else {
                    let Some(entry) = st.displays.get_mut(display) else {
                        warn!(target: "webrtc", "Resize for unknown display '{display}' ignored.");
                        return;
                    };
                    entry.width = w;
                    entry.height = h;
                }
            }
            self.reconfigure_displays().await;
            if display == "primary" {
                self.primary_sized.send_replace(true);
            }
            return;
        }
        self.st.lock().primary_dims = None;
        self.resize_primary_display(res).await;
        self.primary_sized.send_replace(true);
    }

    /// The geometry an admin's manual-resolution lock pins the desktop to, derived on every read
    /// from the server settings, so the lock cannot drift with client state.
    pub(crate) fn server_locked_dims(&self) -> Option<(i32, i32)> {
        if !self.policy.lock().manual_resolution {
            return None;
        }
        let (width, height) = {
            let s = settings();
            (s.manual_width(), s.manual_height())
        };
        if let Some(dims) = self.st.lock().manual_dims {
            return Some(dims);
        }
        let width = if width <= 0 { 1024 } else { width } as i32;
        let height = if height <= 0 { 768 } else { height } as i32;
        Some((width & !1, height & !1))
    }

    /// Resize the primary-only display to a client's request, keep the capture in sync with what
    /// was realized, and tell the client the realized size when it differs. Only an admin's lock
    /// blocks it. Idempotent: clients re-assert their resolution on reconnects, and re-applying the
    /// current size would churn RandR or restart the capture for nothing; the last request counts
    /// as applied too, or a request the realized size differs from would read as pending forever.
    pub(crate) async fn resize_primary_display(&self, res: &str) {
        if self.server_locked_dims().is_some() {
            warn!(target: "webrtc", "Client attempted to resize to {res} but server is in manual resolution mode. Request ignored.");
            return;
        }
        let Some(pipeline) = self.pipeline("primary") else {
            return;
        };
        let Some((mut target_w, mut target_h)) = parse_resize_dims(res) else {
            error!(target: "webrtc", "Invalid resize request: {res}. Ignoring");
            self.st.lock().last_resize_success = false;
            return;
        };
        if self
            .st
            .lock()
            .displays
            .get("primary")
            .is_some_and(|d| d.force_aligned_resolution)
        {
            (target_w, target_h) = align_dims_16(target_w, target_h);
        }
        let (width, height, region) = pipeline.geometry();
        {
            let st = self.st.lock();
            if st.last_resize_success
                && ((width, height) == (target_w, target_h)
                    || st.last_resize_request == Some((target_w, target_h)))
            {
                debug!(target: "webrtc", "Redundant resize request for primary to {target_w}x{target_h}. No action.");
                return;
            }
        }
        info!(target: "webrtc", "Resize requested for display 'primary' with resolution: {target_w}x{target_h}");
        if self.wayland() {
            pipeline.set_geometry(target_w, target_h, region);
            if pipeline.is_screen_capturing() {
                self.size_wayland_screen(target_w, target_h, true).await;
                pipeline.restart_screen().await;
                self.settle_wayland_primary().await;
            }
            let mut st = self.st.lock();
            st.last_resize_success = true;
            st.last_resize_request = Some((target_w, target_h));
            drop(st);
            let (w, h, _) = pipeline.geometry();
            info!(target: "webrtc", "Wayland capture resized to {w}x{h} (requested {target_w}x{target_h})");
            return;
        }
        let mut rects = HashMap::new();
        rects.insert("primary".to_string(), (0, 0, target_w, target_h));
        self.retarget_ahead_of_root(&rects, target_w, target_h)
            .await;
        let realized = self
            .deps
            .display
            .resize_display(&format!("{target_w}x{target_h}"), pipeline.framerate())
            .await;
        match realized {
            Some((realized_w, realized_h)) => {
                if (realized_w, realized_h) != (target_w, target_h) {
                    info!(target: "webrtc", "resize_display realized {realized_w}x{realized_h} for request {target_w}x{target_h}");
                }
                let (stream_w, stream_h) = if (realized_w, realized_h) != (target_w, target_h)
                    && realized_w >= target_w
                    && realized_h >= target_h
                {
                    // A mode rounded past the request (RandR's 8-pixel cells) streams the requested
                    // size from its origin, so the page draws it 1:1 rather than scaled by the
                    // rounding.
                    pipeline
                        .update_capture_region(0, 0, target_w, target_h)
                        .await;
                    (target_w, target_h)
                } else {
                    pipeline.follow_root().await;
                    (realized_w, realized_h)
                };
                let (_, _, region) = pipeline.geometry();
                pipeline.set_geometry(stream_w, stream_h, region);
                {
                    let mut st = self.st.lock();
                    st.last_resize_success = true;
                    st.last_resize_request = Some((target_w, target_h));
                    if let Some(d) = st.displays.get_mut("primary") {
                        d.width = stream_w;
                        d.height = stream_h;
                    }
                }
                self.engine
                    .send_remote_resolution(&format!("{stream_w}x{stream_h}"), "primary");
            }
            None => {
                error!(target: "webrtc", "resize_display('{target_w}x{target_h}') reported failure");
                self.st.lock().last_resize_success = false;
            }
        }
        pipeline.revive_screen().await;
    }

    /// Grow first, shrink after: before the X11 root changes to `root_w`x`root_h`, move each live
    /// capture of a pinned region the new root would not hold to its new rectangle in `rects`,
    /// growing the root first where that rectangle lies past the current one. A capture that follows
    /// the root is left to follow it.
    pub(crate) async fn retarget_ahead_of_root(
        &self,
        rects: &HashMap<String, (i32, i32, i32, i32)>,
        root_w: i32,
        root_h: i32,
    ) {
        let mut moves: Vec<CaptureMove> = Vec::new();
        let mut order: Vec<&String> = rects.keys().collect();
        order.sort_by_key(|d| (*d != "primary", (*d).clone()));
        for did in order {
            let Some(pipeline) = self.pipeline(did) else {
                continue;
            };
            let (w, h, region) = pipeline.geometry();
            let Some((x, y)) = region.filter(|_| pipeline.is_screen_capturing()) else {
                continue;
            };
            if x + w > root_w || y + h > root_h {
                moves.push((did.clone(), pipeline, rects[did]));
            }
        }
        if moves.is_empty() {
            return;
        }
        let Some((cur_w, cur_h)) = self.deps.display.root_size().await else {
            return;
        };
        let need_w = moves
            .iter()
            .map(|(_, _, r)| r.0 + r.2)
            .fold(cur_w, i32::max);
        let need_h = moves
            .iter()
            .map(|(_, _, r)| r.1 + r.3)
            .fold(cur_h, i32::max);
        let (need_w, need_h) = if (need_w, need_h) != (cur_w, cur_h)
            && !self.deps.display.grow_framebuffer(need_w, need_h).await
        {
            (cur_w, cur_h)
        } else {
            (need_w, need_h)
        };
        for (did, pipeline, (x, y, w, h)) in moves {
            let (w, h) = (w.min(need_w - x), h.min(need_h - y));
            if w > 0 && h > 0 {
                debug!(target: "webrtc", "Capture '{did}' moves to {w}x{h}+{x}+{y} ahead of the root's change");
                pipeline.update_capture_region(x, y, w, h).await;
            }
        }
    }

    /// Refuse a secondary display the display server cannot realize: unregister it, stop its
    /// pipeline, and close its peers with a fatal verdict (4000), so the client does not re-register
    /// in a loop. Caller holds `display_lock`.
    async fn drop_secondary(&self, display: &str, reason: &str) {
        let pipeline = self.unregister_secondary(display);
        if let Some(primary) = self.st.lock().layouts.get_mut("primary") {
            primary.x = 0;
            primary.y = 0;
        }
        if let Some(pipeline) = pipeline {
            pipeline.stop().await;
        }
        if self.wayland() {
            self.deps.display.wayland_destroy_output(display).await;
            let attached: Vec<String> = self
                .st
                .lock()
                .displays
                .keys()
                .filter(|d| *d != "primary")
                .cloned()
                .collect();
            self.deps.display.retire_session_screens(&attached).await;
        }
        let doomed: Vec<String> = self
            .engine
            .peers
            .lock()
            .iter()
            .filter(|(_, p)| p.display_id == display)
            .map(|(id, _)| id.clone())
            .collect();
        for peer in &doomed {
            self.close_peer_signaling(peer, 4000, reason);
        }
        self.engine.close_display_peers(display);
        error!(target: "webrtc", "Secondary display '{display}' dropped: {reason}");
    }

    /// Lay the extended desktop out for the connected displays and point each display's capture at
    /// its region, for the primary plus one secondary.
    ///
    /// With no laid-out secondary the plain full-screen capture is restored. Otherwise the primary
    /// size comes from its last layout-path request, else its pipeline dimensions or the live
    /// root, passes the auto-resize feedback clamp, and the dual layout is realized. A layout the
    /// server cannot realize takes the secondary down with it, since an input channel left connected
    /// would keep feeding a display with no laid-out region. A new secondary's pipeline is built from
    /// its own settings, and a bring-up failure drops it so the next pass retries.
    pub fn reconfigure_displays(&self) -> futures::future::BoxFuture<'_, ()> {
        Box::pin(async move {
            {
                let _guard = self.display_lock.lock().await;
                self.reconfigure_locked().await;
            }
            self.broadcast_display_config();
        })
    }

    async fn reconfigure_locked(&self) {
        let wayland = self.wayland();
        let secondary = {
            let st = self.st.lock();
            st.displays
                .iter()
                .find(|(d, e)| *d != "primary" && e.width > 0 && e.height > 0)
                .map(|(d, e)| (d.clone(), e.width, e.height, e.position.clone()))
        };
        let Some(primary_pipeline) = self.pipeline("primary") else {
            return;
        };
        let Some((did, sec_w, sec_h, position)) = secondary else {
            self.reconfigure_alone(&primary_pipeline, wayland).await;
            return;
        };
        let count = self.st.lock().displays.len();
        self.deps.display.ensure_window_manager(count).await;
        let dims = self.st.lock().primary_dims;
        let dims = match dims {
            Some(dims) => dims,
            None => {
                let (mut w, mut h, _) = primary_pipeline.geometry();
                if wayland
                    && let Some(realized) = primary_pipeline
                        .realized_geometry(wayland_output_id(Some("primary")))
                        .await
                {
                    (w, h, _) = realized;
                }
                if w <= 0 || h <= 0 {
                    if wayland {
                        error!(target: "webrtc", "Cannot determine primary display size; aborting layout.");
                        return;
                    }
                    match self.deps.display.root_size().await {
                        Some(root) => (w, h) = root,
                        None => {
                            error!(target: "webrtc", "Cannot determine primary display size; aborting layout.");
                            return;
                        }
                    }
                }
                (w, h)
            }
        };
        let dims = {
            let st = self.st.lock();
            clamp_primary_feedback(dims, Some(&st.layouts).filter(|l| !l.is_empty()), &position)
        };
        self.st.lock().primary_dims = Some(dims);
        let (mut layouts, total_w, total_h) = compute_dual_layout(dims, (sec_w, sec_h), &position);
        if let Some(rect) = layouts.remove("secondary") {
            layouts.insert(did.clone(), rect);
        }
        if wayland {
            // The compositor refuses an output that overlaps the primary's screen, and the screen
            // gives room up only once the primary's capture carries the smaller rectangle: a
            // primary that narrows or shortens where it stands takes its rectangle, and has its
            // screen fitted, before the secondary is placed in the room it frees. A screen the
            // rectangle outgrows in the other dimension is grown ahead of the capture, which it
            // must hold.
            let previous = self.st.lock().layouts.get("primary").copied();
            if let Some(p) = layouts.get("primary").copied() {
                let (w, h, _) = primary_pipeline.geometry();
                let origin = previous.map_or((0, 0), |l| (l.x, l.y));
                if origin == (p.x, p.y) && (p.w < w || p.h < h) {
                    if p.w > w || p.h > h {
                        self.size_wayland_screen(p.w, p.h, true).await;
                    }
                    primary_pipeline
                        .update_capture_region(p.x, p.y, p.w, p.h)
                        .await;
                    self.settle_wayland_primary().await;
                }
            }
            if let Some(refusal) = self.apply_wayland_extension(&did, &mut layouts).await {
                self.drop_secondary(&did, &refusal).await;
                return;
            }
        } else {
            let requested: HashMap<String, (i32, i32)> = layouts
                .iter()
                .map(|(d, l)| (d.clone(), (l.w, l.h)))
                .collect();
            let rects: HashMap<String, (i32, i32, i32, i32)> = layouts
                .iter()
                .map(|(d, l)| (d.clone(), (l.x, l.y, l.w, l.h)))
                .collect();
            self.retarget_ahead_of_root(&rects, total_w, total_h).await;
            let fps = primary_pipeline.framerate();
            if !self
                .deps
                .display
                .apply_extended_layout(&mut layouts, total_w, total_h, fps)
                .await
                || !layouts.contains_key(&did)
            {
                self.drop_secondary(
                    &did,
                    "The X server cannot extend the desktop to fit this display.",
                )
                .await;
                return;
            }
            // Only what the root cut down: a rectangle the layout itself derived must not feed the
            // next pass as its own input.
            let mut st = self.st.lock();
            for (fitted_id, fitted) in &layouts {
                if requested.get(fitted_id) == Some(&(fitted.w, fitted.h)) {
                    continue;
                }
                if fitted_id == "primary" {
                    st.primary_dims = Some((fitted.w, fitted.h));
                } else if let Some(client) = st.displays.get_mut(fitted_id) {
                    client.width = fitted.w;
                    client.height = fitted.h;
                }
            }
        }
        self.st.lock().layouts = layouts.clone();
        let Some(p) = layouts.get("primary").copied() else {
            return;
        };
        if wayland {
            let (w, h, _) = primary_pipeline.geometry();
            if (w, h) != (p.w, p.h) {
                primary_pipeline
                    .update_capture_region(p.x, p.y, p.w, p.h)
                    .await;
                self.push_wayland_realized_geometry("primary", &primary_pipeline)
                    .await;
            }
        } else {
            primary_pipeline
                .update_capture_region(p.x, p.y, p.w, p.h)
                .await;
            primary_pipeline.revive_screen().await;
        }
        let Some(s) = layouts.get(&did).copied() else {
            return;
        };
        self.bring_up_secondary(&did, s, wayland).await;
        if !wayland {
            self.push_layout_geometry(&layouts);
        } else {
            self.deps.display.schedule_session_screen_layout(&layouts);
        }
    }

    /// No secondary is laid out: restore the plain full-screen capture.
    async fn reconfigure_alone(&self, pipeline: &Arc<dyn MediaPipeline>, wayland: bool) {
        let had_layout = !self.st.lock().layouts.is_empty();
        if !had_layout {
            let dims = self.st.lock().primary_dims;
            if let Some((w, h)) = dims {
                self.resize_primary_display(&format!("{w}x{h}")).await;
            }
            return;
        }
        let (p_w, p_h) = {
            let mut st = self.st.lock();
            st.layouts.clear();
            let dims = st.primary_dims.take();
            let (w, h, _) = pipeline.geometry();
            dims.unwrap_or((w, h))
        };
        if wayland {
            let mut empty = Layouts::new();
            let scale = pipeline.scale();
            self.deps
                .display
                .wayland_layout_before_start(&mut empty, &HashSet::new(), scale)
                .await;
            let (w, h, _) = pipeline.geometry();
            pipeline.set_geometry(p_w, p_h, None);
            if (w, h) != (p_w, p_h) && pipeline.is_running() {
                pipeline.restart_screen().await;
            }
            self.size_wayland_screen(p_w, p_h, false).await;
            return;
        }
        let mut rects = HashMap::new();
        rects.insert("primary".to_string(), (0, 0, p_w, p_h));
        self.retarget_ahead_of_root(&rects, p_w, p_h).await;
        // Before the shrink, so no monitor lingers outside the framebuffer.
        self.deps.display.retire_displays().await;
        let realized = self
            .deps
            .display
            .resize_display(&format!("{p_w}x{p_h}"), pipeline.framerate())
            .await;
        let (p_w, p_h) = realized.unwrap_or((p_w, p_h));
        pipeline.set_geometry(p_w, p_h, None);
        if pipeline.is_running() {
            pipeline.restart_screen().await;
        }
        pipeline.revive_screen().await;
        let mut layouts = Layouts::new();
        layouts.insert("primary".to_string(), Layout::new(0, 0, p_w, p_h));
        self.push_layout_geometry(&layouts);
    }

    /// Wayland: realize the extension as compositor outputs; the refusal, if any.
    async fn apply_wayland_extension(&self, did: &str, layouts: &mut Layouts) -> Option<String> {
        let keep: HashSet<String> = {
            let st = self.st.lock();
            st.pipelines
                .iter()
                .filter(|(_, p)| p.is_screen_capturing())
                .map(|(d, _)| d.clone())
                .collect()
        };
        let scale = self.pipeline("primary").map_or(1.0, |p| p.scale());
        let before = self
            .deps
            .display
            .wayland_layout_before_start(layouts, &keep, scale)
            .await;
        if let Some(dropped) = before.dropped.iter().find(|d| d.display_id == did) {
            return Some(dropped.reason.clone());
        }
        let dpis: HashMap<String, String> = {
            let st = self.st.lock();
            st.display_dpis
                .iter()
                .map(|(d, dpi)| (d.clone(), dpi.to_string()))
                .collect()
        };
        let after = self
            .deps
            .display
            .wayland_layout_after_primary(layouts, &dpis)
            .await;
        if let Some(dropped) = after.dropped.iter().find(|d| d.display_id == did) {
            return Some(dropped.reason.clone());
        }
        None
    }

    /// Start a secondary's pipeline, or point its running one at its rectangle.
    async fn bring_up_secondary(&self, did: &str, s: Layout, wayland: bool) {
        let existing = self.pipeline(did);
        let Some(pipeline) = existing else {
            let (config, dpi) = {
                let st = self.st.lock();
                (
                    Self::pipeline_config(&st, did, s.w, s.h, Some((s.x, s.y))),
                    st.display_dpis.get(did).copied(),
                )
            };
            let host: Weak<dyn PipelineHost> = self.me.clone();
            let pipeline = self.deps.pipelines.pipeline(config, None, host);
            // The scale ladder runs for this display's own screen rather than copying whatever the
            // primary was left with.
            let scale = if wayland {
                let dpi = dpi.map_or_else(|| "96".to_string(), |d| d.to_string());
                self.deps
                    .display
                    .realize_wayland_dpi(&dpi, did, Some((s.w, s.h)))
                    .await
            } else {
                self.pipeline("primary").map_or(1.0, |p| p.scale())
            };
            pipeline.set_scale(scale);
            self.st
                .lock()
                .pipelines
                .insert(did.to_string(), pipeline.clone());
            if let Err(e) = pipeline.start(true, false).await {
                error!(target: "webrtc", "Secondary display '{did}' pipeline failed to start ({e}); will retry on next reconfigure.");
                self.st.lock().pipelines.remove(did);
                if wayland {
                    self.drop_secondary(
                        did,
                        "The compositor could not start a capture for this display.",
                    )
                    .await;
                }
                return;
            }
            if wayland {
                if !self.wayland_capture_live(did, &pipeline).await {
                    let last = self.wayland_capture_last_error(did, &pipeline).await;
                    self.drop_secondary(
                        did,
                        &last.unwrap_or_else(|| {
                            "The compositor could not start a capture for this display (encoder session or GPU resources exhausted).".to_string()
                        }),
                    )
                    .await;
                    return;
                }
                self.push_wayland_realized_geometry(did, &pipeline).await;
            }
            info!(target: "webrtc", "Secondary display '{did}' pipeline started at {{'x': {}, 'y': {}, 'w': {}, 'h': {}}}", s.x, s.y, s.w, s.h);
            return;
        };
        let (w, h, region) = pipeline.geometry();
        // A Wayland restart is a full capture reconfigure: skip it when the region is unchanged and
        // the capture is verifiably live.
        let unchanged = wayland
            && (w, h) == (s.w, s.h)
            && region == Some((s.x, s.y))
            && self.wayland_capture_live(did, &pipeline).await;
        if !unchanged {
            pipeline.update_capture_region(s.x, s.y, s.w, s.h).await;
            if wayland {
                self.push_wayland_realized_geometry(did, &pipeline).await;
            }
        }
        pipeline.revive_screen().await;
    }

    /// Tell each laid-out display's pages the size the X11 layout pass gave it: the root may not fit
    /// the request, and a page whose display the server cut down would otherwise re-assert its
    /// requested size forever.
    fn push_layout_geometry(&self, layouts: &Layouts) {
        for (did, rect) in layouts {
            if rect.w > 0 && rect.h > 0 {
                self.engine
                    .send_remote_resolution(&format!("{}x{}", rect.w, rect.h), did);
            }
        }
    }

    /// Record where a display's page draws its stream on the user's desktop, rebroadcast with the
    /// layout, since a page maps a drag that crossed onto a neighbor through the neighbor's box.
    /// Ignored for an unknown display or an impossible box; one box a page publishes is one broadcast
    /// to every client, so a page alternating two is held to one every 0.2 s.
    pub fn set_client_stream_box(
        &self,
        display: &str,
        origin_x: f64,
        origin_y: f64,
        scale_x: f64,
        scale_y: f64,
    ) {
        if !(0.05..=100.0).contains(&scale_x)
            || !(0.05..=100.0).contains(&scale_y)
            || origin_x.abs() > 100_000.0
            || origin_y.abs() > 100_000.0
        {
            return;
        }
        let now = selkies_rtc::rtp::clock::monotonic();
        {
            let mut st = self.st.lock();
            if !st.layouts.contains_key(display) {
                return;
            }
            let Some(entry) = st.displays.get_mut(display) else {
                return;
            };
            let stream_box = (origin_x, origin_y, scale_x, scale_y);
            if entry.client_stream_box == Some(stream_box)
                || (entry.client_stream_box.is_some() && now - entry.client_stream_box_at < 0.2)
            {
                return;
            }
            entry.client_stream_box = Some(stream_box);
            entry.client_stream_box_at = now;
        }
        self.broadcast_display_config();
    }
}
