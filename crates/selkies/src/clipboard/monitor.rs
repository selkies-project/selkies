/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The outbound clipboard monitor: the session's copies, as they happen, to the clients.
//!
//! Event-driven on every rung that offers events (the XFixes monitor, the capture compositor's
//! clipboard sink, the app compositor's data-control watch), with `xclip` polling as the last
//! X11 fallback and a two-second poll of a nested compositor that offers no watch; each pass
//! reads the selection, compares against the echo baseline, and broadcasts real changes. A
//! broadcast runs beside the loop rather than inside it: a multi-megabyte payload takes
//! seconds to cross a link, and a copy made meanwhile has to reach the clients as it happens,
//! superseding the one still in flight, since a client writes its local clipboard only while
//! the user is still there to allow it. The first consumer after a consumer-less stretch gets
//! the current selection once, since change events during that stretch were skipped.
//!
//! The compositor sink is armed once per monitor start, never on an idle timeout: pixelflux
//! keeps the sink across capture stops and starts, and every registration stages a fresh read
//! of the whole current selection, which is delivered but is not a copy anyone made. The
//! data-control watch is armed inside the loop, so a nested session appearing after startup,
//! or restarting on a new socket, is picked up, and an X11 desktop's Xwayland is watched from
//! the moment the server answers. A selection that goes empty, or unreadable, after a secret
//! was broadcast is broadcast as an empty secret, so the clients take the secret back as the
//! session's clipboard lets it go.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use log::{debug, error, info, warn};

use super::{Clipboard, Delivery, Entry, Payload, X11Monitor, native_payload};

/// A tick of the loop: the longest any rung waits for an event before it looks around.
const TICK: Duration = Duration::from_secs(2);
/// Re-reads the monitor gives one selection-change edge whose read came back empty, before
/// treating the selection as genuinely empty.
const REREAD_ATTEMPTS: u32 = 2;
const REREAD_DELAY: Duration = Duration::from_millis(250);

/// What a pass found: whether the selection changed, whether an edge (not a poll) said so,
/// and the compositor delivery when that is what fired.
struct Pass {
    changed: bool,
    from_edge: bool,
    native: Option<Vec<Entry>>,
}

impl Clipboard {
    /// Spawn the monitor loop on the runtime, keeping its handle for a restart.
    pub fn spawn_monitor(self: &Arc<Self>) {
        let me = self.clone();
        let handle = tokio::spawn(async move { me.run_monitor().await });
        *self.monitor_task.lock() = Some(handle);
    }

    /// Stop the monitor loop and release the X11 monitor's connection and thread; a mode
    /// switch builds a fresh handler and must not leak one monitor per transition.
    pub fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
        self.close_x11_monitor();
        debug!(target: "input", "Stopping clipboard monitor");
    }

    /// Whether the monitor loop is running.
    pub fn monitor_active(&self) -> bool {
        self.monitor_active.load(Ordering::Relaxed)
    }

    /// Ask the loop to stop; it unwinds within a tick.
    pub fn request_stop(&self) {
        self.running.store(false, Ordering::Relaxed);
    }

    /// Change the binary clipboard policy and restart the monitor if it is running.
    pub async fn set_binary_clipboard(self: &Arc<Self>, enabled: bool) {
        let _serial = self.binary_lock.lock().await;
        if self.policy.lock().binary == enabled {
            return;
        }
        info!(target: "input", "Binary clipboard setting changing to: {enabled}. Restarting monitor.");
        self.policy.lock().binary = enabled;
        let task = self.monitor_task.lock().take();
        if let Some(task) = task
            && !task.is_finished()
        {
            self.stop();
            task.abort();
            let _ = task.await;
            self.monitor_active.store(false, Ordering::Relaxed);
            self.spawn_monitor();
        }
    }

    /// Register the compositor clipboard sink; the delivery queue, or None when the backend
    /// is not up. Each distinct failure is said once, and the arm retried per tick.
    fn arm_wayland_native(self: &Arc<Self>) -> Option<Arc<Delivery<Vec<Entry>>>> {
        let comp = self.compositor.as_ref()?;
        let queue: Arc<Delivery<Vec<Entry>>> = Arc::new(Delivery::default());
        let weak = Arc::downgrade(self);
        let q = queue.clone();
        let sink = Arc::new(move |entries: Vec<Entry>| {
            if let Some(me) = weak.upgrade() {
                me.state.lock().wl_native_last = Some(entries.clone());
            }
            q.push(entries);
        });
        match comp.set_clipboard_sink(sink) {
            Ok(()) => {
                self.state.lock().wl_native_arm_failure = None;
                info!(target: "input", "Wayland clipboard: native compositor callback active (no polling).");
                Some(queue)
            }
            Err(e) => {
                let repeat = {
                    let mut st = self.state.lock();
                    let repeat = st.wl_native_arm_failure.as_deref() == Some(e.as_str());
                    st.wl_native_arm_failure = Some(e.clone());
                    repeat
                };
                if !repeat {
                    warn!(target: "input", "Wayland clipboard: native callback failed to arm ({e}); retrying.");
                }
                None
            }
        }
    }

    /// Selection-change signals from the app compositor over the data-control client; the
    /// signal queue, or None when no watch could be armed, in which case the loop polls that
    /// compositor and retries the arm each tick. The watch itself only reports a failed
    /// handshake on its own thread, so the same handshake is made here first: a compositor
    /// that offers neither `ext-` nor `zwlr-data-control` is known at arm time instead of
    /// leaving a watch that never fires. Each failure is logged once.
    pub(super) async fn arm_app_compositor_watch(&self) -> Option<Arc<Delivery<Vec<String>>>> {
        let comp = self.compositor.clone()?;
        let display = self.session.app_display();
        let armed = {
            let display = display.clone();
            tokio::task::spawn_blocking(move || {
                comp.clipboard_types_app(&display)?;
                let queue: Arc<Delivery<Vec<String>>> = Arc::new(Delivery::default());
                let q = queue.clone();
                let watch = Arc::new(move |mimes: Vec<String>| q.push(mimes));
                comp.clipboard_watch_app(&display, watch)?;
                Ok::<_, String>(queue)
            })
            .await
            .unwrap_or_else(|e| Err(e.to_string()))
        };
        match armed {
            Ok(queue) => {
                self.state.lock().app_watch_failure = None;
                info!(target: "input", "Wayland clipboard: app-compositor data-control watch active (no forks).");
                Some(queue)
            }
            Err(e) => {
                let failure = (display.clone(), e.clone());
                let repeat = {
                    let mut st = self.state.lock();
                    let repeat = st.app_watch_failure.as_ref() == Some(&failure);
                    st.app_watch_failure = Some(failure);
                    repeat
                };
                if !repeat {
                    warn!(
                        target: "input",
                        "Wayland clipboard: no selection watch on app compositor '{display}' ({e}); polling it instead."
                    );
                }
                self.session.invalidate_app_display_if_dead(&display);
                None
            }
        }
    }

    /// Wait for whichever signals first: the X server's selection-owner change or a
    /// compositor delivery.
    async fn wait_x11_or_compositor(
        x11: &X11Monitor,
        queue: &Delivery<Vec<Entry>>,
    ) -> (bool, Option<Vec<Entry>>) {
        tokio::select! {
            raised = x11.changed().wait(TICK) => {
                if raised {
                    x11.changed().take();
                }
                (raised, None)
            }
            item = queue.recv(TICK) => match item {
                Some(entries) => (true, Some(entries)),
                None => (false, None),
            },
        }
    }

    /// Run the outbound clipboard monitor until `stop`. A mode switch stops the running
    /// monitor and starts the replacement immediately, so the singleton guard waits briefly
    /// for the stopped loop to unwind instead of refusing, which would leave the session with
    /// no outbound clipboard until the setting is toggled.
    pub async fn run_monitor(self: Arc<Self>) {
        if !self.policy().outbound() {
            info!(target: "input", "Skipping outbound clipboard service.");
            return;
        }
        for _ in 0..50 {
            if !self.monitor_active.load(Ordering::Relaxed) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if self.monitor_active.swap(true, Ordering::Relaxed) {
            debug!(target: "input", "Clipboard monitor already running; not starting a second instance.");
            return;
        }
        debug!(target: "input", "Clipboard monitor running (binary mode: {})", self.policy().binary);
        self.running.store(true, Ordering::Relaxed);
        self.monitor_loop().await;
        self.running.store(false, Ordering::Relaxed);
        self.monitor_active.store(false, Ordering::Relaxed);
        debug!(target: "input", "Clipboard monitor stopped");
    }

    async fn monitor_loop(self: &Arc<Self>) {
        let wayland = self.is_wayland();
        let mut x11 = self.ensure_x11_monitor().await;
        let mut wl_native_queue = if wayland && !self.has_separate_app_compositor() {
            self.arm_wayland_native()
        } else {
            None
        };
        // Registering the sink stages the current selection, so the first delivery after an
        // arm is not a copy anyone made.
        let mut wl_native_armed = wl_native_queue.is_some();
        let mut app_watch: Option<(Arc<Delivery<Vec<String>>>, String)> = None;
        let mut first_pass = true;
        let mut had_consumers = false;
        let mut reread_pending = 0u32;
        let mut secret_out = false;
        while self.running.load(Ordering::Relaxed) {
            let separate = wayland && self.has_separate_app_compositor();
            if separate {
                if let Some(monitor) = x11.take() {
                    // A nested session compositor appeared: its XWM bridges its own Xwayland,
                    // so the app-compositor watch takes over.
                    monitor.close();
                    *self.x11.monitor.lock() = None;
                }
                let display = self.session.app_display();
                if app_watch.as_ref().is_none_or(|(_, d)| *d != display)
                    && let Some(queue) = self.arm_app_compositor_watch().await
                {
                    app_watch = Some((queue, display));
                }
            } else if wayland && wl_native_queue.is_none() {
                // Direct mode reached only now (a nested compositor died or never appeared):
                // arm the compositor sink late.
                wl_native_queue = self.arm_wayland_native();
                wl_native_armed = wl_native_queue.is_some();
            }
            if wayland && x11.is_none() {
                x11 = self.ensure_x11_monitor().await;
            }
            let pass = if first_pass {
                first_pass = false;
                Pass {
                    changed: true,
                    from_edge: false,
                    native: None,
                }
            } else if reread_pending > 0 {
                // A change edge whose read came back empty: the new owner had not settled.
                // Dropping it loses that copy until the user makes another.
                reread_pending -= 1;
                tokio::time::sleep(REREAD_DELAY).await;
                Pass {
                    changed: true,
                    from_edge: false,
                    native: None,
                }
            } else if let Some(monitor) = x11.clone() {
                if !monitor.alive() {
                    // Rebuilt in place so outbound clipboard heals on its own instead of
                    // staying dead until a setting is toggled.
                    warn!(target: "input", "X11 clipboard monitor thread exited; respawning.");
                    monitor.close();
                    *self.x11.monitor.lock() = None;
                    x11 = self.ensure_x11_monitor().await;
                    if x11.is_none() {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        Pass {
                            changed: false,
                            from_edge: false,
                            native: None,
                        }
                    } else {
                        // Republished on the fresh monitor; the baseline still dedupes.
                        Pass {
                            changed: true,
                            from_edge: false,
                            native: None,
                        }
                    }
                } else if let Some(queue) = &wl_native_queue {
                    let (changed, native) = Self::wait_x11_or_compositor(&monitor, queue).await;
                    Pass {
                        changed,
                        from_edge: changed && native.is_none(),
                        native,
                    }
                } else {
                    let changed = monitor.changed().wait(TICK).await;
                    if changed {
                        monitor.changed().take();
                    }
                    Pass {
                        changed,
                        from_edge: changed,
                        native: None,
                    }
                }
            } else if let Some(queue) = wl_native_queue.as_ref().filter(|_| !separate) {
                // Not re-armed on idle: re-registering would stage a full selection read
                // every tick.
                match queue.recv(TICK).await {
                    Some(entries) => Pass {
                        changed: true,
                        from_edge: false,
                        native: Some(entries),
                    },
                    None => Pass {
                        changed: false,
                        from_edge: false,
                        native: None,
                    },
                }
            } else if let Some((queue, display)) = &app_watch {
                match queue.recv(TICK).await {
                    // No mimes is a cleared selection: nothing to settle and re-read.
                    Some(mimes) => Pass {
                        changed: true,
                        from_edge: !mimes.is_empty(),
                        native: None,
                    },
                    None => {
                        // A watch on a dead compositor never fires again; a dead socket drops
                        // the detection so the loop re-arms.
                        if self.session.invalidate_app_display_if_dead(display)
                            || self.session.app_display() != *display
                        {
                            app_watch = None;
                        }
                        Pass {
                            changed: false,
                            from_edge: false,
                            native: None,
                        }
                    }
                }
            } else if separate {
                // Poll rung (no data-control watch); the arm above is retried every tick and
                // takes over the moment one holds.
                tokio::time::sleep(TICK).await;
                Pass {
                    changed: true,
                    from_edge: false,
                    native: None,
                }
            } else if wayland {
                // Neither watch armed yet (compositor briefly absent).
                tokio::time::sleep(Duration::from_millis(500)).await;
                Pass {
                    changed: false,
                    from_edge: false,
                    native: None,
                }
            } else {
                // Poll rung; the XFixes rung is re-probed on its cooldown, so an X server
                // that answers later upgrades back to events.
                x11 = self.ensure_x11_monitor().await;
                tokio::time::sleep(Duration::from_millis(500)).await;
                Pass {
                    changed: true,
                    from_edge: false,
                    native: None,
                }
            };
            if !self.running.load(Ordering::Relaxed) {
                break;
            }
            // Taken here, not at the comparison: a delivery dropped for want of consumers
            // still spends the arm's staged read.
            let staged = pass.native.is_some() && std::mem::replace(&mut wl_native_armed, false);
            let has_consumers = self.host.has_consumers();
            let changed = pass.changed || (has_consumers && !had_consumers);
            had_consumers = has_consumers;
            if !changed || !has_consumers {
                continue;
            }
            let use_binary = self.policy().binary;
            let current: Option<Payload> = if let Some(entries) = &pass.native {
                native_payload(entries, use_binary)
            } else if let Some(monitor) = x11.clone() {
                match tokio::task::spawn_blocking(move || monitor.read(use_binary)).await {
                    Ok(Ok(payload)) => payload,
                    Ok(Err(e)) => {
                        error!(target: "input", "Error in clipboard monitor loop: {e}");
                        None
                    }
                    Err(e) => {
                        error!(target: "input", "Error in clipboard monitor loop: {e}");
                        None
                    }
                }
            } else {
                self.read(use_binary).await
            };
            let mut current = current;
            if current.is_none() && pass.from_edge {
                reread_pending = REREAD_ATTEMPTS;
            }
            if current.is_none() && secret_out && reread_pending == 0 {
                current = Some(Payload::secret(""));
            }
            // A watch reports this server's own write back as a selection change like any
            // other, so that write spends one edge, whatever the edge read, and every later
            // one is someone's copy.
            let self_write = (pass.native.is_none() && x11.is_none() && pass.from_edge)
                .then(|| self.state.lock().self_write.take());
            let Some(payload) = current else { continue };
            // The baseline exists to swallow this server's own write coming back. A delivery
            // that is neither that write nor an arm's staged read is a copy someone made, and
            // a client whose first attempt at those bytes failed is waiting for it -- but only
            // the rungs that can tell the two apart say so, since a poll has nothing but the
            // bytes to go on.
            let recopied = if pass.native.is_some() {
                !staged
            } else if let Some(monitor) = &x11 {
                pass.from_edge && !monitor.owns_selection()
            } else if let Some(self_write) = self_write {
                self_write.as_ref() != Some(&payload.bytes)
            } else {
                false
            };
            if recopied || self.baseline().as_ref() != Some(&payload.bytes) {
                debug!(target: "input", "Clipboard changed. Sending content ({})", payload.mime);
                self.set_baseline(&payload);
                secret_out = payload.secret && !payload.is_empty();
                let me = self.clone();
                tokio::spawn(async move { me.send(payload, None, None).await });
            }
        }
    }
}
