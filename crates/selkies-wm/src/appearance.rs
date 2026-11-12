/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The session's appearance under the Selkies window manager: the toolkit settings and the
//! Settings portal that carry the desktop's theme to every application
//! ([`selkies_draw::appearance`]), and the light or dark scheme of the browser the session is
//! streamed to. They are kept here rather than in the shell so they hold in both session modes:
//! an application alone gets the same scheme and the same portal as one on the desktop.
//!
//! [`start`] writes the toolkit settings and takes the portal's name on the session bus before
//! the session's application starts, so an application finds the portal answering from its first
//! read. A thread of its own then watches the desktop's configuration as the frames do
//! ([`ThemeWatch`]), writing the toolkit settings again and signaling the portal as it changes.
//! [`browser_scheme`] queues the scheme the primary display's browser asks for, and the thread
//! puts it into the configuration while the desktop follows the browser, and again when
//! following is turned back on, the newest scheme winning. Writing the file, running `gsettings`,
//! and the portal's calls all happen on that thread, never on the caller's.

use std::sync::OnceLock;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Instant;

use log::{debug, info, warn};
use selkies_draw::appearance::portal::Portal;
use selkies_draw::appearance::toolkits::{self, Decision};
use selkies_draw::appearance::{browser_scheme_changes, follows_browser};
use selkies_draw::style;

use crate::session::SessionEnv;
use crate::theme::ThemeWatch;

/// The light or dark scheme a browser asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrowserScheme {
    /// The light scheme.
    Light,
    /// The dark scheme.
    Dark,
}

impl BrowserScheme {
    /// The scheme's name, as the log states it.
    pub fn name(self) -> &'static str {
        match self {
            BrowserScheme::Light => "light",
            BrowserScheme::Dark => "dark",
        }
    }
}

/// Where [`browser_scheme`] queues schemes for the appearance's thread, once [`start`] ran.
static QUEUE: OnceLock<Sender<BrowserScheme>> = OnceLock::new();

/// Hand the appearance the scheme the primary display's browser asks for. Returns at once.
///
/// # Returns
/// Whether the appearance runs to take it; before [`start`] the scheme is dropped.
pub fn browser_scheme(scheme: BrowserScheme) -> bool {
    QUEUE.get().is_some_and(|queue| queue.send(scheme).is_ok())
}

/// Keep the session's appearance for as long as the process runs (module docblock). A second
/// call does nothing.
///
/// # Arguments
/// * `env` - The session's environment, whose bus the portal is served on and `gsettings` writes
///   through; the device interposers it preloads into applications are left out of the tools'.
pub fn start(env: &SessionEnv) {
    let (queue, schemes) = mpsc::channel();
    if QUEUE.set(queue).is_err() {
        return;
    }
    let mut vars = env.vars();
    vars.retain(|(key, _)| key != "LD_PRELOAD");
    let watch = ThemeWatch::new();
    let decision = apply_toolkits(&watch, &vars);
    let portal = match env.get("DBUS_SESSION_BUS_ADDRESS").filter(|a| !a.is_empty()) {
        Some(address) => Portal::start(&decision, Some(address))
            .inspect_err(|e| debug!(target: "wm", "The Settings portal is not served: {e}"))
            .ok(),
        None => {
            debug!(target: "wm", "The Settings portal is not served: the session has no bus");
            None
        }
    };
    let result = std::thread::Builder::new()
        .name("selkies-wm-appearance".to_string())
        .spawn(move || run(schemes, watch, portal, vars));
    if let Err(e) = result {
        warn!(target: "wm", "The desktop's appearance is not kept: {e}");
    }
}

/// The appearance's thread: takes the browser's schemes and follows the configuration, until
/// the queue closes.
fn run(
    schemes: Receiver<BrowserScheme>,
    mut watch: ThemeWatch,
    mut portal: Option<Portal>,
    vars: Vec<(String, String)>,
) {
    let mut wanted: Option<BrowserScheme> = None;
    let mut following = watch.follows_browser;
    loop {
        let wait = watch.next_poll().saturating_duration_since(Instant::now());
        match schemes.recv_timeout(wait) {
            Ok(scheme) => {
                let scheme = schemes.try_iter().last().unwrap_or(scheme);
                wanted = Some(scheme);
                put_browser_scheme(scheme);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        if !watch.poll(Instant::now()) {
            continue;
        }
        if watch.follows_browser
            && !following
            && let Some(scheme) = wanted
        {
            put_browser_scheme(scheme);
        }
        following = watch.follows_browser;
        let decision = apply_toolkits(&watch, &vars);
        if let Some(portal) = portal.as_mut()
            && let Err(e) = portal.update(&decision)
        {
            debug!(target: "wm", "The Settings portal could not signal its change: {e}");
        }
    }
}

/// Write the toolkit settings the watched configuration implies; the decision written.
fn apply_toolkits(watch: &ThemeWatch, vars: &[(String, String)]) -> Decision {
    let (decision, notes) = toolkits::apply(watch.look.theme, watch.icon_theme.as_deref(), vars);
    for note in notes {
        debug!(target: "wm", "Appearance: {note}");
    }
    decision
}

/// Put the browser's scheme into the desktop's configuration where the desktop follows the
/// browser and is not on that scheme already.
fn put_browser_scheme(scheme: BrowserScheme) {
    let values = style::desktop_values();
    let Some(changes) = browser_scheme_changes(&values, scheme == BrowserScheme::Dark) else {
        if !follows_browser(&values) {
            debug!(target: "wm", "The browser asks for the {} scheme; the desktop keeps its own", scheme.name());
        }
        return;
    };
    match style::save_values(&style::desktop_config_file(), &changes) {
        Ok(()) => info!(target: "wm", "Desktop color scheme set to {}, following the browser", scheme.name()),
        Err(e) => warn!(target: "wm", "Desktop color scheme not set to {}: {e}", scheme.name()),
    }
}
