/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The dedicated thread every blocking X request of the input handler runs on.
//!
//! The thread owns the connection and the XTEST keyboard built on it; the handler hands it jobs
//! over a channel and awaits a reply only where it needs one (a resolve that may fail, a pointer
//! read, a cursor fetch), so the runtime never blocks on the display server. Jobs run in order,
//! which is what keeps a group lock, the key it serves, and a bind's settle in sequence. Between
//! jobs the thread restores a lingering group lock when its linger elapses. A second thread
//! blocks on the connection's event stream and feeds every keymap change back as a job, so the
//! overlay and the keymap cache stay coherent with zero wakeups while nothing happens.
//!
//! A reply the server does not give within `X_REPLY_TIMEOUT` is the handler's cue to abandon
//! this connection and open another (`Dispatcher::reconnect_x`): a server hung under another
//! client's grab, the condition the reconnect exists to survive, would otherwise hold input for
//! the whole outage. Shutting the socket down wakes both threads with an error, and they end,
//! giving up the X client slot, since a dropped connection held by a blocked thread would keep
//! it.

use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use log::debug;
use tokio::sync::oneshot;

use super::x11_keyboard::XTestKeyboard;
use super::x11_link::XLink;

/// X reply wait on the input connection. Generous: no healthy round trip comes near it, so it
/// fires only on an unresponsive server (bounded stall and reconnect).
pub const X_REPLY_TIMEOUT: Duration = Duration::from_secs(20);

/// The state the X thread owns: the keyboard shim over the shared connection.
pub struct XState {
    pub keyboard: XTestKeyboard<Arc<dyn XLink>>,
}

impl XState {
    /// The connection, for pointer, cursor, and query jobs.
    pub fn link(&self) -> &Arc<dyn XLink> {
        self.keyboard.link()
    }
}

/// One unit of work for the X thread.
type Job = Box<dyn FnOnce(&mut XState) + Send>;

/// The handle the handler keeps to its X thread.
#[derive(Clone)]
pub struct XWorker {
    tx: mpsc::Sender<Job>,
    link: Arc<dyn XLink>,
}

/// A shared connection is the connection: the X thread, the event thread, and the handle hold one.
impl<T: XLink + ?Sized> XLink for Arc<T> {
    fn keycode_range(&self) -> (u8, u8) {
        (**self).keycode_range()
    }
    fn keysym_to_keycode(&self, keysym: u32) -> u8 {
        (**self).keysym_to_keycode(keysym)
    }
    fn keycode_to_keysym(&self, keycode: u8, level: usize) -> u32 {
        (**self).keycode_to_keysym(keycode, level)
    }
    fn keyboard_mapping(&self, first: u8, count: u8) -> Result<Vec<Vec<u32>>, String> {
        (**self).keyboard_mapping(first, count)
    }
    fn modifier_mapping(&self) -> Result<Vec<u8>, String> {
        (**self).modifier_mapping()
    }
    fn change_keyboard_mapping(&self, first: u8, rows: &[Vec<u32>]) -> Result<(), String> {
        (**self).change_keyboard_mapping(first, rows)
    }
    fn refresh_keyboard_mapping(&self, first: u8, count: u8) -> Result<(), String> {
        (**self).refresh_keyboard_mapping(first, count)
    }
    fn fake_key(&self, keycode: u8, down: bool) {
        (**self).fake_key(keycode, down)
    }
    fn fake_motion(&self, x: i16, y: i16, relative: bool) {
        (**self).fake_motion(x, y, relative)
    }
    fn fake_button(&self, button: u8, down: bool) {
        (**self).fake_button(button, down)
    }
    fn flush(&self) {
        (**self).flush()
    }
    fn sync(&self) -> Result<(), String> {
        (**self).sync()
    }
    fn has_xkb(&self) -> bool {
        (**self).has_xkb()
    }
    fn xkb_placement(
        &self,
    ) -> Result<std::collections::HashMap<u32, super::x11_link::XkbPlacement>, String> {
        (**self).xkb_placement()
    }
    fn xkb_locked_group(&self) -> Result<u8, String> {
        (**self).xkb_locked_group()
    }
    fn xkb_lock_group(&self, group: u8) {
        (**self).xkb_lock_group(group)
    }
    fn query_pointer(&self) -> Result<(i16, i16, u16), String> {
        (**self).query_pointer()
    }
    fn cursor_image(&self) -> Result<super::x11_link::XCursorImage, String> {
        (**self).cursor_image()
    }
    fn screen_dpi(&self) -> Option<f64> {
        (**self).screen_dpi()
    }
    fn autorepeat(&self) -> Option<(u16, u16)> {
        (**self).autorepeat()
    }
    fn reads_kernel_input(&self) -> bool {
        (**self).reads_kernel_input()
    }
    fn wait_keymap_event(&self) -> Option<super::x11_link::KeymapEvent> {
        (**self).wait_keymap_event()
    }
    fn broken(&self) -> bool {
        (**self).broken()
    }
    fn close(&self) {
        (**self).close()
    }
}

impl XWorker {
    /// Start the thread over `link`, building the keyboard shim on it, and the event thread
    /// beside it where the link delivers events.
    pub fn spawn(link: Arc<dyn XLink>) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel::<Job>();
        let handle_link = link.clone();
        let events_link = link.clone();
        let events_tx = tx.clone();
        thread::Builder::new()
            .name("x-input".into())
            .spawn(move || {
                let mut state = XState {
                    keyboard: XTestKeyboard::new(link),
                };
                loop {
                    let wait = state
                        .keyboard
                        .group_restore_due()
                        .map(|at| at.saturating_duration_since(Instant::now()))
                        .unwrap_or(Duration::from_secs(3600));
                    match rx.recv_timeout(wait) {
                        Ok(job) => job(&mut state),
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                    state.keyboard.restore_group_if_due();
                }
                state.keyboard.release_group_lock();
                state.link().close();
                debug!(target: "input", "X input thread ended");
            })?;
        thread::Builder::new()
            .name("x-input-events".into())
            .spawn(move || {
                while let Some(event) = events_link.wait_keymap_event() {
                    let sent = events_tx.send(Box::new(move |state: &mut XState| {
                        state.keyboard.keymap_changed(event)
                    }));
                    if sent.is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            tx,
            link: handle_link,
        })
    }

    /// Whether the connection has broken, read without a job, so a hung thread cannot hide it.
    pub fn broken(&self) -> bool {
        self.link.broken()
    }

    /// Give the connection up now: a job blocked on a reply wakes with an error, so a hung
    /// server cannot hold the thread for the rest of its outage.
    pub fn abandon(&self) {
        self.link.close();
    }

    /// Queue a job and move on.
    pub fn send(&self, job: impl FnOnce(&mut XState) + Send + 'static) -> bool {
        self.tx.send(Box::new(job)).is_ok()
    }

    /// Run a job and await its result; None when the thread is gone or the reply did not come
    /// within `X_REPLY_TIMEOUT`.
    pub async fn call<R: Send + 'static>(
        &self,
        job: impl FnOnce(&mut XState) -> R + Send + 'static,
    ) -> Option<R> {
        let (tx, rx) = oneshot::channel();
        let sent = self.tx.send(Box::new(move |state: &mut XState| {
            let _ = tx.send(job(state));
        }));
        if sent.is_err() {
            return None;
        }
        match tokio::time::timeout(X_REPLY_TIMEOUT, rx).await {
            Ok(Ok(result)) => Some(result),
            _ => None,
        }
    }

    /// Wait until every job queued before now has run.
    pub async fn drain(&self) {
        let _ = self.call(|_| ()).await;
    }
}
