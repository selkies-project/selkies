/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Clipboard synchronization both ways between the session and its pages.
//!
//! Both transports hand their clipboard verbs to one [`Clipboard`], so policy and fallback
//! behavior stay identical across modes. Reads and writes of the session's selection follow
//! fallback ladders that resolve the newest, event-driven mechanism first and degrade rung by
//! rung, with cooldowns that re-probe the top rung rather than latching into a fallback:
//!
//! - X11: the XFixes selection monitor on a dedicated connection ([`x11::X11Monitor`]: owner
//!   changes as events, conversions with INCR, writes served natively on CLIPBOARD and
//!   PRIMARY), then `xclip` polling ([`xclip`]) while the monitor cannot be built, re-probed
//!   every ten seconds.
//! - Wayland: the capture compositor's own selection through pixelflux's clipboard sink
//!   (every copy delivered with its flavors, no polling) and `set_clipboard`; under a nested
//!   session compositor the apps' copies land on that compositor's selection instead, watched
//!   and read through pixelflux's data-control client ([`CompositorClipboard`]), polled on a
//!   two-second cadence while no watch can be armed. A rootful Xwayland on the capture
//!   compositor bridges no selection of its own, so its X11 desktop's copies are watched with
//!   the XFixes monitor on that display as well. The path is subprocess-free: nothing here
//!   forks `wl-copy`, `wtype`, or their like.
//!
//! The readable targets are one set in one precedence order on every rung, since a target
//! offered on one path must be readable on all of them: a password manager's hint first
//! (`SECRET_HINTS`, a copy marked `secret` reads as its text alone), then the markup
//! (`text/html`), the pictures (`image/png` first), a file manager's `text/uri-list` resolved
//! to the picture it names, and the plain text. A copy whose markup carries text is text, and
//! its picture is left out; one whose markup is only an `img` tag is the picture
//! ([`flavors::markup_has_text`]). Markup travels with the plain text beneath it as one
//! envelope under [`flavors::FLAVORS_MIME`], so a paste into a rich editor takes the markup
//! and one into a plain field the text the source wrote.
//!
//! Writes offer every flavor a copy carried. A client's picture in another format is offered
//! as PNG as well, first, since that is what most applications paste ([`png::png_beside`]);
//! the conversion is waited for only briefly, a slow one joining the clipboard when ready
//! unless something newer took it, and a paste chord pressed before then waits for it. A
//! secret is offered with the hint that marked it, so the session's clipboard history skips
//! it again, and the mark rides to the clients ahead of the payload (`clipboard_secret`),
//! which keep it out of sight; an empty secret tells them the session let the secret go.
//!
//! Echo suppression: the bytes this server writes become the change baseline before the write,
//! so the ownership change that write raises never re-broadcasts a client's own content, and
//! a client's push of what the session clipboard already holds (its plain text the same but
//! for line ends) is left to its owner. A delivery that is neither this server's write nor an
//! arm's staged read is a copy someone made, identical bytes or not, which a client whose
//! first attempt failed is waiting for.
//!
//! Outbound, a copy is announced to every connected controller and collaborator, never a
//! viewer, in frames sized for latency rather than capacity ([`outbound::CHUNK_SIZE`]) through
//! the host's bulk gauge; a newer copy supersedes one still crossing the link, which then gets
//! no further chunks. A client's fetch (`cr`) is answered to that client alone behind a
//! `clipboard_reply` tag and is neither superseded nor supersedes. Inbound, a multipart
//! transfer is bounded in declared and accumulated size ([`verbs::MULTIPART_MAX_BYTES`]).
//!
//! Policy: `enable_clipboard` is `true`, `false`, `in` (client to session only), or `out`
//! (session to client only); `enable_binary_clipboard` gates pictures in both directions, never
//! the markup envelope, which is text ([`Policy`]).
//!
//! The typing route for compositors without a virtual keyboard (KWin) rides on the same
//! ladders: the text is written as a secret, pasted with Shift+Insert, and the previous
//! clipboard restored once the focused application has fetched it ([`paste`]).

pub mod flavors;
pub mod monitor;
pub mod outbound;
pub mod paste;
pub mod png;
pub mod verbs;
pub mod x11;
pub mod xclip;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use log::{debug, info, warn};
use parking_lot::Mutex;
use tokio::sync::watch;
use tokio::task::JoinHandle;

pub use flavors::{Entry, FLAVORS_MIME};
pub use outbound::{ClientId, ClipboardHost};
pub use paste::PasteKeys;
pub use x11::X11Monitor;

/// The target a password manager offers beside a secret it copies, valued `secret`
/// (KeePassXC, KDE), in KDE's spelling and the prefixed ones a toolkit that validates mime
/// types can see.
pub const SECRET_HINTS: [&str; 3] = [
    "x-kde-passwordManagerHint",
    "text/x-kde-passwordManagerHint",
    "application/x-kde-passwordManagerHint",
];

/// The image targets read and offered, in precedence order.
pub const IMAGE_MIMES: [&str; 6] = [
    "image/png",
    "image/jpeg",
    "image/bmp",
    "image/webp",
    "image/svg+xml",
    "image/svg",
];

/// The text targets a data-control selection is read under, in precedence order.
const APP_TEXT_MIMES: [&str; 5] = [
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];

/// How long a client's clipboard write waits for the PNG of an image sent in another
/// format: the client's later messages, its input among them, wait too.
pub const PNG_WAIT: Duration = Duration::from_millis(250);
/// How long a paste chord waits for that PNG when it is still being made
/// (`Clipboard::paste_waits_for_png`): only the chord waits, the rest of the input does not.
pub const PASTE_WAIT: Duration = Duration::from_secs(5);
/// Ctrl+V and Shift+Insert: the keysym pasted and the modifiers either needs held.
pub const PASTE_CHORDS: [(u32, [u32; 2]); 3] = [
    (0x76, [0xffe3, 0xffe4]),
    (0x56, [0xffe3, 0xffe4]),
    (0xff63, [0xffe1, 0xffe2]),
];
/// Backoff after a failed XFixes monitor build, so pollers do not hammer a dead display.
const X11_MONITOR_RETRY: Duration = Duration::from_secs(10);

/// Whether a hint target's value marks its copy secret.
pub fn secret_hint(value: &[u8]) -> bool {
    let trimmed: &[u8] = {
        let is_pad = |b: &u8| matches!(*b, 0 | b' ' | b'\t' | b'\r' | b'\n');
        let start = value.iter().position(|b| !is_pad(b)).unwrap_or(value.len());
        let end = value
            .iter()
            .rposition(|b| !is_pad(b))
            .map_or(start, |i| i + 1);
        &value[start..end]
    };
    trimmed == b"secret"
}

/// Text with LF line ends and no trailing line end: the form two clipboards holding the same
/// text agree on, whatever line ends each keeps.
fn same_lines(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        match text[i] {
            b'\r' => {
                out.push(b'\n');
                if text.get(i + 1) == Some(&b'\n') {
                    i += 1;
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    while out.last() == Some(&b'\n') {
        out.pop();
    }
    out
}

/// One clipboard content as it travels: its bytes, its type, and whether its owner marked
/// it secret.
///
/// Text is `text/plain`; markup with the text beneath it is an envelope under
/// [`FLAVORS_MIME`]; a picture carries its own mime. A secret is text alone, since markup or
/// a picture beside it would leave the secret where the mark cannot follow; an empty secret
/// tells the clients that the session's clipboard no longer holds the one sent before it.
/// `Debug` shows neither a secret's text nor its length, so a log line printing a payload
/// keeps it out of sight.
#[derive(Clone, PartialEq, Eq)]
pub struct Payload {
    pub mime: String,
    pub bytes: Bytes,
    pub secret: bool,
}

impl Payload {
    /// Plain text.
    pub fn text(text: impl Into<String>) -> Payload {
        Payload {
            mime: "text/plain".into(),
            bytes: Bytes::from(text.into()),
            secret: false,
        }
    }

    /// Text its owner marked secret.
    pub fn secret(text: impl Into<String>) -> Payload {
        Payload {
            mime: "text/plain".into(),
            bytes: Bytes::from(text.into()),
            secret: true,
        }
    }

    /// Bytes under their mime: a picture, or an envelope.
    pub fn binary(mime: impl Into<String>, bytes: impl Into<Bytes>) -> Payload {
        Payload {
            mime: mime.into(),
            bytes: bytes.into(),
            secret: false,
        }
    }

    /// Markup and the text beneath it as one envelope.
    pub fn envelope(entries: &[Entry]) -> Payload {
        Payload::binary(FLAVORS_MIME, flavors::envelope(entries))
    }

    /// Whether this is plain text (a secret included).
    pub fn is_text(&self) -> bool {
        self.mime == "text/plain"
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// The bytes as text, invalid sequences replaced.
    pub fn text_lossy(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
}

impl fmt::Debug for Payload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.secret {
            return f.write_str(if self.bytes.is_empty() {
                "SecretText('')"
            } else {
                "SecretText(<hidden>)"
            });
        }
        write!(f, "Payload({}, {} bytes)", self.mime, self.bytes.len())
    }
}

/// The clipboard policy gates: `enable_clipboard` by direction and the binary switch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    /// `true`, `false`, `in` (client to session), or `out` (session to client).
    pub clipboard: String,
    /// Whether pictures move at all (`enable_binary_clipboard`).
    pub binary: bool,
}

impl Policy {
    /// The policy the settings resolved.
    pub fn from_settings() -> Policy {
        let s = crate::settings::settings();
        Policy {
            clipboard: s.enable_clipboard().to_string(),
            binary: s.enable_binary_clipboard().on,
        }
    }

    /// Whether a client's content may reach the session.
    pub fn inbound(&self) -> bool {
        matches!(self.clipboard.as_str(), "true" | "in")
    }

    /// Whether the session's content may reach the clients.
    pub fn outbound(&self) -> bool {
        matches!(self.clipboard.as_str(), "true" | "out")
    }
}

/// What the clipboard needs to know about the session it serves; the input module, which
/// detects the app compositor, implements it. `app_display` and
/// `has_separate_app_compositor` are called from the runtime on every tick and per read, so
/// they answer from a cache and rescan on a throttle.
pub trait Session: Send + Sync + 'static {
    /// Whether the session runs on the Wayland backend.
    fn is_wayland(&self) -> bool;

    /// Socket of the compositor applications run under, the target for the clipboard: the
    /// capture compositor's for a plain session, the nested session compositor's otherwise.
    fn app_display(&self) -> String;

    /// Whether apps live under a compositor distinct from pixelflux's own, so its selection
    /// never reaches them.
    fn has_separate_app_compositor(&self) -> bool;

    /// Re-detect the app compositor when its socket no longer answers (a nested compositor
    /// that died or restarted under another name); a live socket that failed one request is
    /// kept. True when the resolution was dropped, which ends a watch armed on that socket
    /// even where the compositor comes back under the same name.
    fn invalidate_app_display_if_dead(&self, display: &str) -> bool;

    /// The X display the X11 backend's monitor watches and `xclip` reads, `$DISPLAY`.
    fn x11_display(&self) -> Option<String> {
        std::env::var("DISPLAY")
            .ok()
            .map(|d| d.trim().to_string())
            .filter(|d| !d.is_empty())
    }

    /// Display of an X server whose selections nothing bridges into the Wayland session: on
    /// the Wayland backend without a nested session compositor, a live Xwayland on `$DISPLAY`
    /// is a rootful (or unmanaged) client of the capture compositor hosting an X11 desktop,
    /// and Xwayland bridges no selection by itself. A nested compositor's own XWM bridges its
    /// Xwayland into the selection already watched, and on the X11 backend the display is the
    /// session itself, so both answer None. The server must be an Xwayland, not a leftover
    /// Xvfb or Xorg that merely holds the display number.
    fn x11_session_display(&self) -> Option<String> {
        if !self.is_wayland() || self.has_separate_app_compositor() {
            return None;
        }
        let name = self.x11_display()?;
        if !x11::display_live(&name) || !x11::display_is_xwayland(&name) {
            return None;
        }
        Some(name)
    }

    /// The unbridged X server of a Wayland session is being watched from now on: the pages
    /// are told again what a second display can do.
    fn x11_session_adopted(&self) {}
}

/// pixelflux's clipboard surface as the Wayland rungs call it: the capture compositor's
/// selection (sink and offer) and the data-control client to an app compositor's socket.
/// Implemented for [`pixelflux::ScreenCapture`]; errors are the message pixelflux gave.
pub trait CompositorClipboard: Send + Sync + 'static {
    /// Register the consumer of what client applications copy to the capture compositor's
    /// clipboard; every registration stages a read of the whole current selection.
    fn set_clipboard_sink(
        &self,
        sink: Arc<dyn pixelflux::sink::ClipboardSink>,
    ) -> Result<(), String>;
    /// Offer `entries` on the capture compositor's selection.
    fn set_clipboard(&self, entries: Vec<Entry>) -> Result<(), String>;
    /// Mimes the app compositor's current selection offers, none for nothing copied.
    fn clipboard_types_app(&self, display: &str) -> Result<Vec<String>, String>;
    /// The app compositor selection's payload for `mime`, None when it offers no such target.
    fn clipboard_read_app(&self, display: &str, mime: &str) -> Result<Option<Vec<u8>>, String>;
    /// Take the app compositor's selection, serving `entries` until another client copies.
    fn clipboard_write_app(&self, display: &str, entries: Vec<Entry>) -> Result<(), String>;
    /// The mimes the selection last written has served to pasters, once more than `after`
    /// of them or `timeout` has passed.
    fn clipboard_served_app(&self, after: usize, timeout: Duration) -> Vec<String>;
    /// Drop the app compositor's selection.
    fn clipboard_clear_app(&self, display: &str) -> Result<(), String>;
    /// Report every selection change of the app compositor to `watch`.
    fn clipboard_watch_app(
        &self,
        display: &str,
        watch: Arc<dyn pixelflux::sink::SelectionWatch>,
    ) -> Result<(), String>;
    /// Stop the selection watch for `display`.
    fn clipboard_unwatch_app(&self, display: &str);
}

impl CompositorClipboard for pixelflux::ScreenCapture {
    fn set_clipboard_sink(
        &self,
        sink: Arc<dyn pixelflux::sink::ClipboardSink>,
    ) -> Result<(), String> {
        pixelflux::ScreenCapture::set_clipboard_sink(self, sink).map_err(|e| e.to_string())
    }

    fn set_clipboard(&self, entries: Vec<Entry>) -> Result<(), String> {
        pixelflux::ScreenCapture::set_clipboard(self, entries).map_err(|e| e.to_string())
    }

    fn clipboard_types_app(&self, display: &str) -> Result<Vec<String>, String> {
        pixelflux::ScreenCapture::clipboard_types_app(self, display.to_string())
            .map_err(|e| e.to_string())
    }

    fn clipboard_read_app(&self, display: &str, mime: &str) -> Result<Option<Vec<u8>>, String> {
        pixelflux::ScreenCapture::clipboard_read_app(self, display.to_string(), mime.to_string())
            .map_err(|e| e.to_string())
    }

    fn clipboard_write_app(&self, display: &str, entries: Vec<Entry>) -> Result<(), String> {
        pixelflux::ScreenCapture::clipboard_write_app(self, display.to_string(), entries)
            .map_err(|e| e.to_string())
    }

    fn clipboard_served_app(&self, after: usize, timeout: Duration) -> Vec<String> {
        pixelflux::ScreenCapture::clipboard_served_app(self, after, timeout.as_secs_f64())
    }

    fn clipboard_clear_app(&self, display: &str) -> Result<(), String> {
        pixelflux::ScreenCapture::clipboard_clear_app(self, display.to_string())
            .map_err(|e| e.to_string())
    }

    fn clipboard_watch_app(
        &self,
        display: &str,
        watch: Arc<dyn pixelflux::sink::SelectionWatch>,
    ) -> Result<(), String> {
        pixelflux::ScreenCapture::clipboard_watch_app(self, display.to_string(), watch)
            .map_err(|e| e.to_string())
    }

    fn clipboard_unwatch_app(&self, display: &str) {
        pixelflux::ScreenCapture::clipboard_unwatch_app(self, display.to_string())
    }
}

/// The converter a client's non-PNG picture goes through; a field so a test can slow it.
type PngConverter = fn(&str, &[u8]) -> Option<Vec<u8>>;

/// The baseline and caches the ladders share.
#[derive(Default)]
struct State {
    /// Change-detection baseline shared by the monitor and the writes: content this server
    /// just wrote is never re-broadcast, and the baseline survives client reconnects so
    /// nothing is resent unchanged.
    last_bytes: Option<Bytes>,
    /// The baseline's type, which says where its plain text is.
    last_mime: Option<String>,
    /// Bytes this server last wrote to the session clipboard, pending the one
    /// selection-change edge that write raises; spent there.
    self_write: Option<Bytes>,
    /// The PNG still being made for a client's image, which a paste waits for; set once done.
    png_beside: Option<watch::Receiver<bool>>,
    /// The capture compositor's last delivered selection, for on-demand reads: the compositor
    /// does not echo its own selection back.
    wl_native_last: Option<Vec<Entry>>,
    /// Last `(display, error)` the app-compositor watch failed with, reported once.
    app_watch_failure: Option<(String, String)>,
    /// Last `(display, error)` the app-compositor read failed with, reported once.
    app_read_failure: Option<(String, String)>,
    /// Last error the compositor clipboard sink failed to arm with, reported once.
    wl_native_arm_failure: Option<String>,
}

/// The XFixes rung's slot and its backoff.
struct X11Rung {
    monitor: Mutex<Option<Arc<X11Monitor>>>,
    retry_at: Mutex<Option<Instant>>,
    unavail_logged: AtomicBool,
    /// Keeps concurrent callers from racing two monitors into existence.
    build_lock: tokio::sync::Mutex<()>,
}

/// The clipboard handler both transports share: the verbs, the read and write ladders, the
/// outbound monitor, and the paste-chord typing route.
///
/// Built once per transport service with the host that carries frames to the clients, the
/// session's topology, and the pixelflux surface on Wayland. Blocking X and compositor work
/// runs on blocking threads; the runtime only queues, dispatches, and awaits.
pub struct Clipboard {
    host: Arc<dyn ClipboardHost>,
    session: Arc<dyn Session>,
    compositor: Option<Arc<dyn CompositorClipboard>>,
    policy: Mutex<Policy>,
    state: Mutex<State>,
    x11: X11Rung,
    xclip: xclip::Xclip,
    inbound: Mutex<verbs::Multipart>,
    requests: Mutex<verbs::Requests>,
    /// Per-client send serialization and the newest announcement each client was given.
    sends: Mutex<outbound::Sends>,
    announcements: AtomicU64,
    /// Whether the monitor loop should keep running.
    running: AtomicBool,
    /// Singleton guard; one monitor loop runs at a time.
    monitor_active: AtomicBool,
    monitor_task: Mutex<Option<JoinHandle<()>>>,
    /// Serializes `set_binary_clipboard` so its stop and respawn is atomic under rapid toggles.
    binary_lock: tokio::sync::Mutex<()>,
    /// One save, write, paste, restore cycle at a time, with a reentrancy latch so the paste
    /// chord's own key path cannot recurse.
    inject_lock: tokio::sync::Mutex<()>,
    inject_active: AtomicBool,
    png_converter: PngConverter,
}

impl Clipboard {
    /// A handler for `session`, sending to `host`'s clients; `compositor` is pixelflux's
    /// surface on the Wayland backend and None on X11.
    pub fn new(
        host: Arc<dyn ClipboardHost>,
        session: Arc<dyn Session>,
        compositor: Option<Arc<dyn CompositorClipboard>>,
        policy: Policy,
    ) -> Arc<Clipboard> {
        Arc::new(Clipboard {
            host,
            session,
            compositor,
            policy: Mutex::new(policy),
            state: Mutex::new(State::default()),
            x11: X11Rung {
                monitor: Mutex::new(None),
                retry_at: Mutex::new(None),
                unavail_logged: AtomicBool::new(false),
                build_lock: tokio::sync::Mutex::new(()),
            },
            xclip: xclip::Xclip::default(),
            inbound: Mutex::new(verbs::Multipart::default()),
            requests: Mutex::new(verbs::Requests::default()),
            sends: Mutex::new(outbound::Sends::default()),
            announcements: AtomicU64::new(0),
            running: AtomicBool::new(false),
            monitor_active: AtomicBool::new(false),
            monitor_task: Mutex::new(None),
            binary_lock: tokio::sync::Mutex::new(()),
            inject_lock: tokio::sync::Mutex::new(()),
            inject_active: AtomicBool::new(false),
            png_converter: png::png_beside,
        })
    }

    /// The policy in force.
    pub fn policy(&self) -> Policy {
        self.policy.lock().clone()
    }

    fn is_wayland(&self) -> bool {
        self.session.is_wayland()
    }

    fn has_separate_app_compositor(&self) -> bool {
        self.session.has_separate_app_compositor()
    }

    /// The baseline, as the monitor and the request paths compare against it.
    fn baseline(&self) -> Option<Bytes> {
        self.state.lock().last_bytes.clone()
    }

    /// Set the change baseline to a payload the session now holds.
    fn set_baseline(&self, payload: &Payload) {
        let mut st = self.state.lock();
        st.last_bytes = Some(payload.bytes.clone());
        st.last_mime = Some(payload.mime.clone());
    }

    /// Read the session clipboard.
    ///
    /// Wayland is fully native. A rootful Xwayland on the capture compositor bridges no
    /// selection of its own, so the X11 desktop's copies are read from it first (a selection
    /// still owned there is this server's own write, for which the compositor side is
    /// authoritative); then the compositor sink's cache, which holds the capture compositor's
    /// selection and is the wrong session under a separate app compositor, where the
    /// data-control client reads whichever socket the apps use instead. X11 uses the XFixes
    /// monitor with an `xclip` fallback.
    ///
    /// Args:
    ///     use_binary: Prefer image targets (and file-manager uri-lists) before the text.
    ///
    /// Returns the content, or None when nothing is readable.
    pub async fn read(&self, use_binary: bool) -> Option<Payload> {
        if self.is_wayland() {
            if let Some(monitor) = self.ensure_x11_monitor().await
                && !monitor.owns_selection()
            {
                let m = monitor.clone();
                match tokio::task::spawn_blocking(move || m.read(use_binary)).await {
                    Ok(Ok(Some(payload))) => return Some(payload),
                    Ok(Ok(None)) => {}
                    Ok(Err(e)) => {
                        warn!(target: "input", "X11 clipboard read on the Wayland session failed: {e}")
                    }
                    Err(e) => {
                        warn!(target: "input", "X11 clipboard read on the Wayland session failed: {e}")
                    }
                }
            }
            let cached = if self.has_separate_app_compositor() {
                None
            } else {
                self.state.lock().wl_native_last.clone()
            };
            if let Some(entries) = cached
                && let Some(payload) = native_payload(&entries, use_binary)
            {
                return Some(payload);
            }
            return self.app_clipboard_read(use_binary).await;
        }
        if let Some(monitor) = self.ensure_x11_monitor().await {
            match tokio::task::spawn_blocking(move || monitor.read(use_binary)).await {
                Ok(Ok(payload)) => return payload,
                Ok(Err(e)) => {
                    warn!(target: "input", "native X11 clipboard read failed, using xclip: {e}")
                }
                Err(e) => {
                    warn!(target: "input", "native X11 clipboard read failed, using xclip: {e}")
                }
            }
        }
        self.xclip
            .read(use_binary, self.session.x11_display().as_deref())
            .await
    }

    /// Read the selection of the compositor the apps use over the pixelflux data-control
    /// client; None when it is empty or unreadable. A copy its owner marked secret reads as
    /// its text alone. A persistent failure is reported once, not per read.
    async fn app_clipboard_read(&self, use_binary: bool) -> Option<Payload> {
        let comp = self.compositor.clone()?;
        let display = self.session.app_display();
        let read = move |comp: &Arc<dyn CompositorClipboard>,
                         display: &str,
                         use_binary: bool|
              -> Result<Option<Payload>, String> {
            let types = comp.clipboard_types_app(display)?;
            let has = |m: &str| types.iter().any(|t| t == m);
            if let Some(hint) = SECRET_HINTS.iter().find(|h| has(h))
                && comp
                    .clipboard_read_app(display, hint)?
                    .is_some_and(|v| secret_hint(&v))
            {
                let Some(source) = APP_TEXT_MIMES.iter().find(|m| has(m)) else {
                    return Ok(Some(Payload::secret("")));
                };
                return Ok(comp
                    .clipboard_read_app(display, source)?
                    .map(|d| Payload::secret(String::from_utf8_lossy(&d).into_owned())));
            }
            let html = if has("text/html") {
                comp.clipboard_read_app(display, "text/html")?
                    .filter(|h| !h.is_empty())
            } else {
                None
            };
            let text_copy = html.as_deref().is_some_and(flavors::markup_has_text);
            if use_binary
                && !text_copy
                && let Some(target) = IMAGE_MIMES.iter().find(|m| has(m))
                && let Some(data) = comp.clipboard_read_app(display, target)?
                && !data.is_empty()
            {
                return Ok(Some(Payload::binary(*target, data)));
            }
            if let Some(html) = html {
                let plain = ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING"]
                    .iter()
                    .find(|m| has(m))
                    .map(|m| comp.clipboard_read_app(display, m))
                    .transpose()?
                    .flatten()
                    .unwrap_or_default();
                let mut entries = vec![("text/html".to_string(), html)];
                if !plain.is_empty() {
                    entries.push(("text/plain".to_string(), plain));
                }
                return Ok(Some(Payload::envelope(&entries)));
            }
            if let Some(source) = APP_TEXT_MIMES.iter().find(|m| has(m))
                && let Some(data) = comp.clipboard_read_app(display, source)?
            {
                return Ok(Some(Payload::text(
                    String::from_utf8_lossy(&data).into_owned(),
                )));
            }
            Ok(None)
        };
        let d = display.clone();
        let outcome = tokio::task::spawn_blocking(move || read(&comp, &d, use_binary))
            .await
            .unwrap_or_else(|e| Err(e.to_string()));
        match outcome {
            Ok(payload) => {
                self.state.lock().app_read_failure = None;
                payload
            }
            Err(e) => {
                let failure = (display.clone(), e.clone());
                let repeat = {
                    let mut st = self.state.lock();
                    let repeat = st.app_read_failure.as_ref() == Some(&failure);
                    st.app_read_failure = Some(failure);
                    repeat
                };
                if repeat {
                    debug!(target: "input", "data-control clipboard read failed: {e}");
                } else {
                    warn!(target: "input", "data-control clipboard read failed: {e}");
                }
                self.session.invalidate_app_display_if_dead(&display);
                None
            }
        }
    }

    /// Whether a client's push carries nothing but the session clipboard's own plain text,
    /// its line ends aside. The browser hands the session's copy back in its own form
    /// (Chromium on Windows keeps text with CRLF line ends, markup comes back rewrapped), and
    /// writing that would take the clipboard from its owner with less than it offered: a file
    /// manager's cut, a URI list marked as a cut beside its paths as text, would become the
    /// text, and its paste would offer to save the text as a file instead of moving the files.
    /// A secret is always written, for the hint it carries.
    fn echoes_session_clipboard(&self, payload: &Payload, flavors: Option<&[Entry]>) -> bool {
        if payload.secret {
            return false;
        }
        let text: Option<&[u8]> = match flavors {
            Some(entries) => entries
                .iter()
                .find(|(m, _)| m == "text/plain")
                .map(|(_, d)| d.as_slice()),
            None if payload.is_text() => Some(&payload.bytes),
            None => None,
        };
        let Some(text) = text else { return false };
        let st = self.state.lock();
        let (Some(base), Some(base_mime)) = (&st.last_bytes, &st.last_mime) else {
            return false;
        };
        let base: Vec<u8> = if base_mime == FLAVORS_MIME {
            match flavors::flavors(base) {
                Ok(entries) => match entries.into_iter().find(|(m, _)| m == "text/plain") {
                    Some((_, d)) => d,
                    None => return false,
                },
                Err(_) => return false,
            }
        } else if base_mime == "text/plain" {
            base.to_vec()
        } else {
            return false;
        };
        same_lines(text) == same_lines(&base)
    }

    /// Set the session clipboard, native first with forked fallbacks.
    ///
    /// Wayland sets pixelflux's own selection in-process (or writes through the data-control
    /// client to a separate app compositor); X11 offers through the XFixes monitor's
    /// connection, falling back to an `xclip` fork. The written bytes become the monitor
    /// baseline before the write, so the ownership-change event cannot echo a client's own
    /// content back. On the capture compositor the payload is also offered on the unbridged X
    /// server, so the X11 desktop can paste it.
    ///
    /// An image in another format is offered as PNG as well, first, which makes the PNG the
    /// flavor read back; a secret is offered with the hint that marked it. The client's
    /// messages wait for this write, so the conversion is waited for `PNG_WAIT` only: a photo
    /// taking longer is offered as it came at once, and the PNG joins it when ready unless
    /// something newer took the clipboard meanwhile. An envelope restored with no flavors
    /// named is unpacked, so what reaches the selection is the markup and its text, never the
    /// envelope itself. A push of what the session clipboard already holds is left to its
    /// owner (`echoes_session_clipboard`).
    ///
    /// Args:
    ///     payload: The flavor the session reads back and echoes against.
    ///     flavors: Everything the copy carried, offered together; None for the payload alone.
    ///
    /// Returns true when the clipboard was set (an empty payload is a no-op true).
    pub async fn write(self: &Arc<Self>, payload: Payload, flavors: Option<Vec<Entry>>) -> bool {
        let (payload, flavors) = match (payload, flavors) {
            (p, None) if p.mime == FLAVORS_MIME => match flavors::flavors(&p.bytes) {
                Ok(entries) => {
                    let (mime, data) = entries[0].clone();
                    (Payload::binary(mime, data), Some(entries))
                }
                Err(e) => {
                    warn!(target: "input", "clipboard envelope rejected: {e}");
                    return false;
                }
            },
            other => other,
        };
        if self.echoes_session_clipboard(&payload, flavors.as_deref()) {
            debug!(target: "input", "A client sent back what the session clipboard holds; left to its owner.");
            return true;
        }
        if flavors.is_none()
            && !payload.is_empty()
            && payload.mime.starts_with("image/")
            && payload.mime != "image/png"
        {
            let convert = self.png_converter;
            let (mime, data) = (payload.mime.clone(), payload.bytes.clone());
            let mut conversion = tokio::task::spawn_blocking(move || convert(&mime, &data));
            match tokio::time::timeout(PNG_WAIT, &mut conversion).await {
                Err(_) => {
                    let ok = self.set_clipboard(payload.clone(), None, false).await;
                    if ok {
                        let (tx, rx) = watch::channel(false);
                        self.state.lock().png_beside = Some(rx);
                        let me = self.clone();
                        tokio::spawn(async move {
                            me.add_png_beside(conversion, payload, tx).await;
                        });
                    }
                    return ok;
                }
                Ok(Ok(Some(png))) => {
                    let entries = vec![
                        ("image/png".to_string(), png.clone()),
                        (payload.mime.clone(), payload.bytes.to_vec()),
                    ];
                    return self
                        .set_clipboard(Payload::binary("image/png", png), Some(entries), false)
                        .await;
                }
                Ok(_) => {}
            }
        }
        self.set_clipboard(payload, flavors, false).await
    }

    /// Hold a paste chord (Ctrl+V, Shift+Insert) until the PNG of a client's image is on the
    /// clipboard, `PASTE_WAIT` at most. A photo takes a second or more to convert, and until
    /// then the clipboard offers it only as it came (a JPEG), which the session's browsers do
    /// not paste. The transports hand a client's messages over one at a time, so the keys
    /// after the chord wait behind it and keep their order.
    ///
    /// Args:
    ///     keysym: The key going down.
    ///     modifier_held: Whether the client holds a keysym down.
    pub async fn paste_waits_for_png(&self, keysym: u32, modifier_held: impl Fn(u32) -> bool) {
        let Some(mut rx) = self.state.lock().png_beside.clone() else {
            return;
        };
        if *rx.borrow() {
            return;
        }
        let Some((_, mods)) = PASTE_CHORDS.iter().find(|(k, _)| *k == keysym) else {
            return;
        };
        if !mods.iter().any(|m| modifier_held(*m)) {
            return;
        }
        debug!(target: "input", "paste waits for the PNG of the client's image");
        let _ = tokio::time::timeout(PASTE_WAIT, rx.wait_for(|done| *done)).await;
    }

    /// Offer a client's image as PNG as well once its conversion is done, if that image is
    /// still the session's clipboard: a copy made since, by a client or in the session, is
    /// newer and keeps the clipboard.
    async fn add_png_beside(
        self: &Arc<Self>,
        conversion: JoinHandle<Option<Vec<u8>>>,
        original: Payload,
        done: watch::Sender<bool>,
    ) {
        let png = conversion.await.ok().flatten();
        let still_current = self.baseline().as_ref() == Some(&original.bytes)
            && self
                .x11
                .monitor
                .lock()
                .as_ref()
                .is_none_or(|m| m.owns_selection());
        if let Some(png) = png
            && still_current
        {
            let entries = vec![
                ("image/png".to_string(), png.clone()),
                (original.mime.clone(), original.bytes.to_vec()),
            ];
            self.set_clipboard(Payload::binary("image/png", png), Some(entries), true)
                .await;
        }
        let _ = done.send(true);
    }

    /// The write `write` describes, for content offered as it is. `from_png` marks the
    /// deferred PNG offer, which must not cancel its own wait.
    async fn set_clipboard(
        self: &Arc<Self>,
        payload: Payload,
        flavors: Option<Vec<Entry>>,
        from_png: bool,
    ) -> bool {
        if payload.is_empty() {
            return true;
        }
        // Markup reads back as an envelope, so that is the form the baseline has to hold.
        let reads_back = match &flavors {
            Some(entries) if entries.iter().any(|(m, _)| m == "text/html") => {
                let text: Vec<Entry> = entries
                    .iter()
                    .filter(|(m, _)| m == "text/html" || m == "text/plain")
                    .cloned()
                    .collect();
                Payload::envelope(&text)
            }
            _ => payload.clone(),
        };
        {
            let mut st = self.state.lock();
            st.last_bytes = Some(reads_back.bytes.clone());
            st.last_mime = Some(reads_back.mime.clone());
            st.self_write = Some(reads_back.bytes);
            if !from_png {
                st.png_beside = None;
            }
        }
        let mut entries: Vec<Entry> =
            flavors.unwrap_or_else(|| vec![(payload.mime.clone(), payload.bytes.to_vec())]);
        if payload.secret {
            entries.extend(
                SECRET_HINTS
                    .iter()
                    .map(|h| (h.to_string(), b"secret".to_vec())),
            );
        }
        if self.is_wayland() {
            if !self.has_separate_app_compositor() {
                let mut ok = match self.compositor.as_ref() {
                    Some(comp) => match comp.set_clipboard(entries.clone()) {
                        Ok(()) => {
                            self.state.lock().wl_native_last = Some(entries.clone());
                            true
                        }
                        Err(e) => {
                            warn!(target: "input", "native wayland clipboard set failed: {e}");
                            false
                        }
                    },
                    None => {
                        warn!(target: "input", "native wayland clipboard set failed: no compositor");
                        false
                    }
                };
                if let Some(monitor) = self.ensure_x11_monitor().await {
                    match tokio::task::spawn_blocking(move || monitor.offer(entries)).await {
                        Ok(Ok(true)) => ok = true,
                        Ok(Ok(false)) => {}
                        Ok(Err(e)) => {
                            warn!(target: "input", "X11 clipboard offer on the Wayland session failed: {e}")
                        }
                        Err(e) => {
                            warn!(target: "input", "X11 clipboard offer on the Wayland session failed: {e}")
                        }
                    }
                }
                return ok;
            }
            let Some(comp) = self.compositor.clone() else {
                warn!(target: "input", "data-control clipboard write failed: no compositor");
                return false;
            };
            let mut offered: Vec<Entry> = Vec::new();
            for (mime, data) in entries {
                if mime == "text/plain" {
                    offered.extend(APP_TEXT_MIMES.iter().map(|m| (m.to_string(), data.clone())));
                } else {
                    offered.push((mime, data));
                }
            }
            let display = self.session.app_display();
            let d = display.clone();
            let outcome =
                tokio::task::spawn_blocking(move || comp.clipboard_write_app(&d, offered))
                    .await
                    .unwrap_or_else(|e| Err(e.to_string()));
            return match outcome {
                Ok(()) => true,
                Err(e) => {
                    warn!(target: "input", "data-control clipboard write failed: {e}");
                    self.session.invalidate_app_display_if_dead(&display);
                    false
                }
            };
        }
        if let Some(monitor) = self.ensure_x11_monitor().await {
            let offers = entries.clone();
            match tokio::task::spawn_blocking(move || monitor.offer(offers)).await {
                Ok(Ok(true)) => return true,
                Ok(Ok(false)) => {}
                Ok(Err(e)) => {
                    warn!(target: "input", "native X11 clipboard offer failed, using xclip: {e}")
                }
                Err(e) => {
                    warn!(target: "input", "native X11 clipboard offer failed, using xclip: {e}")
                }
            }
        }
        self.xclip
            .write(&payload, self.session.x11_display().as_deref())
            .await
    }

    /// Get-or-create the event-driven X11 monitor, off the runtime's workers: construction
    /// opens its own X connection, and a server disrupted mid-session would stall the loop for
    /// the whole bounded handshake. A failed build backs off `X11_MONITOR_RETRY` before the
    /// next attempt, so pollers do not hammer a dead display, and a display that comes up
    /// later is still re-probed. On the Wayland backend the monitor is built only for an
    /// unbridged X server (`Session::x11_session_display`), on that display, with no backoff
    /// while there is none, since the socket probe is cheap and an Xwayland starting later is
    /// caught within a tick.
    async fn ensure_x11_monitor(&self) -> Option<Arc<X11Monitor>> {
        if let Some(m) = self.x11.monitor.lock().clone() {
            return Some(m);
        }
        if self
            .x11
            .retry_at
            .lock()
            .is_some_and(|at| Instant::now() < at)
        {
            return None;
        }
        let _guard = self.x11.build_lock.lock().await;
        if let Some(m) = self.x11.monitor.lock().clone() {
            return Some(m);
        }
        let display = if self.is_wayland() {
            let session = self.session.clone();
            let probed = tokio::task::spawn_blocking(move || session.x11_session_display());
            Some(probed.await.ok().flatten()?)
        } else {
            self.session.x11_display()
        };
        let name = display.clone();
        let built = tokio::task::spawn_blocking(move || X11Monitor::new(name.as_deref()))
            .await
            .unwrap_or_else(|e| Err(e.to_string()));
        match built {
            Ok(monitor) => {
                let monitor = Arc::new(monitor);
                *self.x11.monitor.lock() = Some(monitor.clone());
                self.x11.unavail_logged.store(false, Ordering::Relaxed);
                let where_ = match (&display, self.is_wayland()) {
                    (Some(d), true) => {
                        format!(" on {d} (unbridged X server of the Wayland session)")
                    }
                    _ => String::new(),
                };
                info!(target: "input", "X11 clipboard: XFixes event monitor active{where_}.");
                if self.is_wayland() {
                    self.session.x11_session_adopted();
                }
                Some(monitor)
            }
            Err(e) => {
                let on = match (&display, self.is_wayland()) {
                    (Some(d), true) => format!(" on {d}"),
                    _ => String::new(),
                };
                let fallback = if self.is_wayland() {
                    "retrying"
                } else {
                    "falling back to polling"
                };
                if self.x11.unavail_logged.swap(true, Ordering::Relaxed) {
                    debug!(target: "input", "X11 clipboard: XFixes monitor unavailable{on} ({e}); {fallback}.");
                } else {
                    info!(target: "input", "X11 clipboard: XFixes monitor unavailable{on} ({e}); {fallback}.");
                }
                *self.x11.retry_at.lock() = Some(Instant::now() + X11_MONITOR_RETRY);
                None
            }
        }
    }

    /// Release the X11 monitor's connection and thread.
    fn close_x11_monitor(&self) {
        if let Some(m) = self.x11.monitor.lock().take() {
            m.close();
        }
    }
}

/// What the capture compositor's own selection reads as, from the flavors its sink delivered:
/// a picture as bytes with its mime where pictures are taken, markup with the text beneath it
/// as one envelope, text as text, and the text alone as a secret where the owner marked the
/// copy secret; None for a picture where none is taken, and for a cleared selection, which is
/// delivered without flavors. A copy whose markup carries text is text, a picture beside it
/// notwithstanding.
pub fn native_payload(entries: &[Entry], use_binary: bool) -> Option<Payload> {
    let hinted = entries
        .iter()
        .any(|(m, d)| SECRET_HINTS.contains(&m.as_str()) && secret_hint(d));
    let entries: Vec<&Entry> = entries
        .iter()
        .filter(|(m, _)| !SECRET_HINTS.contains(&m.as_str()))
        .collect();
    let plain = entries
        .iter()
        .find(|(m, _)| m != "text/html" && !m.starts_with("image/"))
        .map(|(_, d)| d);
    if hinted {
        let text = plain
            .map(|d| String::from_utf8_lossy(d).into_owned())
            .unwrap_or_default();
        return Some(Payload::secret(text));
    }
    let html = entries
        .iter()
        .find(|(m, _)| m == "text/html")
        .map(|(_, d)| d)
        .filter(|h| !h.is_empty());
    let image = entries.iter().find(|(m, _)| m.starts_with("image/"));
    if let Some((mime, data)) = image
        && !html.is_some_and(|h| flavors::markup_has_text(h))
    {
        return use_binary.then(|| Payload::binary(mime.clone(), data.clone()));
    }
    if let Some(html) = html {
        let mut flavors = vec![("text/html".to_string(), html.clone())];
        if let Some(plain) = plain.filter(|p| !p.is_empty()) {
            flavors.push(("text/plain".to_string(), plain.clone()));
        }
        return Some(Payload::envelope(&flavors));
    }
    plain.map(|d| Payload::text(String::from_utf8_lossy(d).into_owned()))
}

/// A one-reader queue of deliveries from a foreign thread, holding the newest `CAP` items:
/// the compositor's clipboard sink and the app compositor's selection watch push from threads
/// of their own, and the monitor loop awaits the next item with a timeout.
pub(crate) struct Delivery<T> {
    items: Mutex<std::collections::VecDeque<T>>,
    notify: tokio::sync::Notify,
}

impl<T> Default for Delivery<T> {
    fn default() -> Self {
        Delivery {
            items: Mutex::new(std::collections::VecDeque::new()),
            notify: tokio::sync::Notify::new(),
        }
    }
}

impl<T> Delivery<T> {
    const CAP: usize = 4;

    /// Queue an item, dropping the oldest when full.
    pub(crate) fn push(&self, item: T) {
        let mut items = self.items.lock();
        if items.len() >= Self::CAP {
            items.pop_front();
        }
        items.push_back(item);
        drop(items);
        self.notify.notify_one();
    }

    /// The next item within `timeout`, else None.
    pub(crate) async fn recv(&self, timeout: Duration) -> Option<T> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(item) = self.items.lock().pop_front() {
                return Some(item);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            if tokio::time::timeout(remaining, self.notify.notified())
                .await
                .is_err()
            {
                return self.items.lock().pop_front();
            }
        }
    }
}

/// A table keyed by client that forgets entries for clients no longer connected.
pub(crate) fn prune<V>(table: &mut HashMap<ClientId, V>, live: &[ClientId]) {
    table.retain(|id, _| live.contains(id));
}
