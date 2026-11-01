/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Clipboard-paste text injection: the typing route for Wayland compositors with no
//! `zwp_virtual_keyboard` (KWin).
//!
//! The text replaces the clipboard, Shift+Insert pastes it, and what was copied before is
//! restored. The chord's two keysyms exist in every base layout, so this needs nothing beyond
//! the clipboard and ordinary key events. Held modifiers are lifted around the chord so they
//! cannot corrupt the paste, the write's baseline keeps the monitor from echoing the injected
//! text back to the clients, and the text is offered as a secret so a clipboard history
//! (Klipper) neither records nor re-offers each typed character. The restore waits for the
//! paste's fetch where the app compositor's selection reports its sends, else a fixed margin.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use log::{debug, error};

use super::{Clipboard, CompositorClipboard, Payload, SECRET_HINTS};

/// How long the focused app has to fetch a pasted text before the clipboard is restored where
/// nothing reports the fetch: Konsole under KWin took 50-80 ms for a session's first paste,
/// and a selection dropped before the fetch pastes nothing.
pub const PASTE_FETCH: Duration = Duration::from_millis(150);
/// Where pixelflux's data-control source reports its sends, the restore follows the fetch
/// itself, waiting this long at most for an app that never pastes.
pub const PASTE_FETCH_MAX: Duration = Duration::from_millis(500);
/// The focused app's margin to receive the new offer before the chord.
const OFFER_SETTLE: Duration = Duration::from_millis(20);
const SHIFT_L: u32 = 0xFFE1;
const INSERT: u32 = 0xFF63;

/// The key injection the paste chord rides on; the input module implements it over its
/// keyboard ladder.
#[async_trait]
pub trait PasteKeys: Send + Sync {
    /// Press or release `keysym`; `neutralize` false keeps held modifiers as they are.
    async fn key(&self, keysym: u32, down: bool, neutralize: bool);

    /// The modifier keysyms the client currently holds.
    fn held_modifiers(&self) -> Vec<u32>;

    /// Whether the client still holds `keysym`.
    fn modifier_held(&self, keysym: u32) -> bool;
}

impl Clipboard {
    /// Type `text` by replacing the clipboard with it, pasting via Shift+Insert, and
    /// restoring what was copied before. True once the paste chord is sent; false when an
    /// injection is already running, the write failed, or a step raised.
    pub async fn inject_text_via_clipboard(
        self: &Arc<Self>,
        text: &str,
        keys: &dyn PasteKeys,
    ) -> bool {
        if self.inject_active.load(Ordering::Relaxed) {
            return false;
        }
        let _serial = self.inject_lock.lock().await;
        self.inject_active.store(true, Ordering::Relaxed);
        let held = keys.held_modifiers();
        for modifier in &held {
            keys.key(*modifier, false, true).await;
        }
        let outcome = self.paste_cycle(text, keys).await;
        for modifier in &held {
            if keys.modifier_held(*modifier) {
                keys.key(*modifier, true, true).await;
            }
        }
        self.inject_active.store(false, Ordering::Relaxed);
        match outcome {
            Ok(sent) => sent,
            Err(e) => {
                error!(target: "input", "Clipboard text injection failed: {e}");
                false
            }
        }
    }

    /// Save, write, paste, restore.
    async fn paste_cycle(
        self: &Arc<Self>,
        text: &str,
        keys: &dyn PasteKeys,
    ) -> Result<bool, String> {
        let previous = self.read(true).await;
        if !self.write(Payload::secret(text), None).await {
            return Ok(false);
        }
        tokio::time::sleep(OFFER_SETTLE).await;
        let sends = self.paste_sends();
        // Reads before the chord (a clipboard history's) are no paste.
        let seen = match &sends {
            Some(comp) => {
                let comp = comp.clone();
                tokio::task::spawn_blocking(move || {
                    comp.clipboard_served_app(0, Duration::ZERO).len()
                })
                .await
                .map_err(|e| e.to_string())?
            }
            None => 0,
        };
        keys.key(SHIFT_L, true, true).await;
        keys.key(INSERT, true, false).await;
        keys.key(INSERT, false, true).await;
        keys.key(SHIFT_L, false, true).await;
        match sends {
            None => tokio::time::sleep(PASTE_FETCH).await,
            Some(comp) => {
                self.paste_fetched(comp, seen).await?;
            }
        }
        match previous {
            Some(old) => {
                self.write(old, None).await;
            }
            None if self.is_wayland() => self.clear_selection().await,
            None => {}
        }
        Ok(true)
    }

    /// pixelflux's report of the mimes the app compositor's selection served, where a paste is
    /// served by it: on Wayland under a separate app compositor.
    fn paste_sends(&self) -> Option<Arc<dyn CompositorClipboard>> {
        if !self.is_wayland() || !self.has_separate_app_compositor() {
            return None;
        }
        self.compositor.clone()
    }

    /// Wait until the selection has served text beyond the `seen` sends, at most
    /// `PASTE_FETCH_MAX`; a read of the secret hint alone is a clipboard history's, not the
    /// paste. True once the text was fetched.
    async fn paste_fetched(
        &self,
        comp: Arc<dyn CompositorClipboard>,
        mut seen: usize,
    ) -> Result<bool, String> {
        let deadline = Instant::now() + PASTE_FETCH_MAX;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(false);
            }
            let c = comp.clone();
            let served = tokio::task::spawn_blocking(move || c.clipboard_served_app(seen, left))
                .await
                .map_err(|e| e.to_string())?;
            if served[seen.min(served.len())..]
                .iter()
                .any(|m| !SECRET_HINTS.contains(&m.as_str()))
            {
                return Ok(true);
            }
            if served.len() <= seen {
                return Ok(false);
            }
            seen = served.len();
        }
    }

    /// Drop the session's selection: what an injection leaves behind when there was nothing to
    /// restore, since an empty clipboard stays empty for the user. The capture compositor does
    /// not echo its own selection back, so its cached copy goes too, or a later fetch would
    /// read the typed text.
    pub async fn clear_selection(&self) {
        let Some(comp) = self.compositor.clone() else {
            return;
        };
        let outcome = if self.has_separate_app_compositor() {
            let display = self.session.app_display();
            tokio::task::spawn_blocking(move || comp.clipboard_clear_app(&display))
                .await
                .unwrap_or_else(|e| Err(e.to_string()))
        } else {
            self.state.lock().wl_native_last = None;
            comp.set_clipboard(vec![("text/plain".to_string(), Vec::new())])
        };
        if let Err(e) = outcome {
            debug!(target: "input", "post-injection clipboard clear failed: {e}");
        }
    }
}
