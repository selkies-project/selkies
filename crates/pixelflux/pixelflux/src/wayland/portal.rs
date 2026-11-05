/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! xdg-desktop-portal client: one `RemoteDesktop` (or `ScreenCast`) session on the session bus
//! that hands out the PipeWire streams of the host's monitors and takes input events for its
//! seat. Compositors without the privileged wlroots protocols (GNOME, KDE) expose both only this
//! way, through their portal backend, so this is the rung host capture falls to when the registry
//! lacks capture or virtual-input globals.
//!
//! Every portal call that ends in a `Response` signal is made through [`PortalSession::request`],
//! which subscribes to the request object the portal will answer on before calling, as the
//! handle-token convention requires; a consent dialog the backend shows blocks that call until
//! the user answers. Input notifications go to an input thread of the session's own, so no
//! caller waits on the bus round trip, and it makes each once the one before it is answered: the
//! frontend handles every method call on a thread pool, so two in flight together reach the
//! backend in either order (a click's release before its press, which leaves the button held).
//! Each public call runs its async zbus calls to completion with
//! [`zbus::block_on`] on the calling thread; between calls zbus drives the connection from a
//! helper thread of its own, so a `Response` that lands meanwhile is queued for the stream that
//! subscribed to it. A restore token the backend hands back is kept under the
//! user's state directory, so a session reopened to change the cursor mode, and every later
//! start on this machine, needs no second consent; a backend that hands out none (KDE) shows
//! no dialog to an unsandboxed app in the first place.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{OnceLock, mpsc};

use zbus::export::futures_core::Stream;
use zbus::{Connection, OwnedObjectPath, OwnedVariant, Proxy, Variant};

const DESKTOP: &str = "org.freedesktop.portal.Desktop";
const DESKTOP_PATH: &str = "/org/freedesktop/portal/desktop";
const REQUEST_IFACE: &str = "org.freedesktop.portal.Request";
const SESSION_IFACE: &str = "org.freedesktop.portal.Session";
const SCREENCAST_IFACE: &str = "org.freedesktop.portal.ScreenCast";
const REMOTE_DESKTOP_IFACE: &str = "org.freedesktop.portal.RemoteDesktop";

pub const DEVICE_KEYBOARD: u32 = 1;
pub const DEVICE_POINTER: u32 = 2;
pub const CURSOR_HIDDEN: u32 = 1;
pub const CURSOR_EMBEDDED: u32 = 2;
pub const CURSOR_METADATA: u32 = 4;
const SOURCE_MONITOR: u32 = 1;
const PERSIST_UNTIL_REVOKED: u32 = 2;

type Options<'a> = HashMap<&'a str, Variant<'a>>;

/// A `Notify*` method for the input thread, and the call that makes it.
type NotifyCall<P = Proxy<'static>> = (&'static str, Box<dyn FnOnce(&P) + Send>);

const MOTION_ABSOLUTE: &str = "NotifyPointerMotionAbsolute";

/// One monitor stream the portal started: its PipeWire node and, where the backend says,
/// its logical position and size in the compositor's coordinate space.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortalStream {
    pub node_id: u32,
    pub position: Option<(i32, i32)>,
    pub size: Option<(i32, i32)>,
}

/// A started portal session: its streams, the devices it may drive, and the token that
/// restores it without a dialog.
pub struct PortalSession {
    conn: Connection,
    screencast: Proxy<'static>,
    remote: Option<Proxy<'static>>,
    session: OwnedObjectPath,
    pub streams: Vec<PortalStream>,
    pub devices: u32,
    pub cursor_mode: u32,
    pub restore_token: Option<String>,
    /// RemoteDesktop interface version, 0 when the session drives no input. `ConnectToEIS`
    /// exists from version 2.
    pub rd_version: u32,
    /// KDE's backend serves the session: it reads a smooth scroll's vertical axis negated.
    kde: bool,
    /// Mutter's remote-desktop service runs on the session bus, which every GNOME backend hands
    /// input to: it reads `NotifyPointerMotionAbsolute` in the stream's pixels, which
    /// xdg-desktop-portal-gnome passes the spec's logical point on as.
    pub mutter: bool,
    /// Whether dropping the session closes it on the portal: not while `open` still runs it
    /// inside its `zbus::block_on`, which a second one inside would panic on.
    armed: bool,
    /// The input thread's queue, from the first `Notify*` call on.
    notify_tx: OnceLock<mpsc::Sender<NotifyCall>>,
}

fn opt_u32(o: &mut Options<'_>, key: &'static str, v: u32) {
    o.insert(key, Variant::U32(v));
}

/// Where the portal's restore token lives between runs: `$XDG_STATE_HOME/pixelflux`, or
/// `~/.local/state/pixelflux` in its absence.
fn token_path() -> Option<std::path::PathBuf> {
    let base = match std::env::var_os("XDG_STATE_HOME") {
        Some(dir) if !dir.is_empty() => std::path::PathBuf::from(dir),
        _ => std::path::PathBuf::from(std::env::var_os("HOME")?).join(".local/state"),
    };
    Some(base.join("pixelflux/portal-restore-token"))
}

/// The restore token saved by an earlier session, if any.
pub fn saved_restore_token() -> Option<String> {
    let token = std::fs::read_to_string(token_path()?).ok()?;
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_string())
}

/// Keep a restore token for the next session (a token is single-use, so the newest replaces
/// the one that opened this session).
pub fn save_restore_token(token: &str) {
    let Some(path) = token_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(&path, token) {
        eprintln!(
            "[HostCapture] portal restore token not saved to {}: {e}",
            path.display()
        );
    }
}

fn value_i32(v: &Variant<'_>) -> Option<i32> {
    match v {
        Variant::I32(x) => Some(*x),
        Variant::U32(x) => Some(*x as i32),
        Variant::Variant(inner) => value_i32(inner),
        _ => None,
    }
}

fn value_pair(v: &Variant<'_>) -> Option<(i32, i32)> {
    match v {
        Variant::Structure(s) if s.fields().len() == 2 => {
            Some((value_i32(&s.fields()[0])?, value_i32(&s.fields()[1])?))
        }
        Variant::Variant(inner) => value_pair(inner),
        _ => None,
    }
}

/// The `a(ua{sv})` stream list of a `Start` response.
fn parse_streams(v: &Variant<'_>) -> Vec<PortalStream> {
    let mut out = Vec::new();
    let Variant::Array(arr) = v else { return out };
    for item in arr.iter() {
        let Variant::Structure(s) = item else {
            continue;
        };
        let fields = s.fields();
        let Some(Variant::U32(node_id)) = fields.first() else {
            continue;
        };
        let mut stream = PortalStream {
            node_id: *node_id,
            position: None,
            size: None,
        };
        if let Some(Variant::Dict(d)) = fields.get(1) {
            for (k, val) in d.iter() {
                let Variant::Str(k) = k else { continue };
                match k.as_str() {
                    "position" => stream.position = value_pair(val),
                    "size" => stream.size = value_pair(val),
                    _ => {}
                }
            }
        }
        out.push(stream);
    }
    out
}

/// The next message of `stream`, or `None` once it ends.
async fn next_message<S: Stream + Unpin>(stream: &mut S) -> Option<S::Item> {
    std::future::poll_fn(|cx| Pin::new(&mut *stream).poll_next(cx)).await
}

/// Whether `name` has an owner on the bus `conn` is on.
async fn name_owned(conn: &Connection, name: &str) -> bool {
    let Ok(bus) = zbus::fdo::DBusProxy::new(conn).await else {
        return false;
    };
    let Ok(name) = zbus::names::BusName::try_from(name) else {
        return false;
    };
    bus.name_has_owner(name).await.unwrap_or(false)
}

impl PortalSession {
    /// Open and start a session. `devices` (`DEVICE_*` bits) selects what input the session
    /// drives — zero opens a plain ScreenCast session; `capture` asks for every monitor as its
    /// own stream with `cursor_mode` (`CURSOR_*`), downgraded to what the portal offers. The
    /// call returns once the portal (and any dialog it shows) has answered; a session the
    /// portal created before a later step failed is closed before the error returns.
    pub fn open(
        capture: bool,
        devices: u32,
        cursor_mode: u32,
        restore_token: Option<&str>,
    ) -> Result<Self, String> {
        zbus::block_on(async {
            let mut this = Self::connect(devices, cursor_mode).await?;
            if let Err(e) = this.start(capture, devices, restore_token).await {
                this.close().await;
                return Err(e);
            }
            this.armed = true;
            Ok(this)
        })
    }

    /// The session bus and the portal interfaces a session with `devices` speaks to, before
    /// any session exists.
    async fn connect(devices: u32, cursor_mode: u32) -> Result<Self, String> {
        let conn = Connection::session()
            .await
            .map_err(|e| format!("session bus: {e}"))?;
        let screencast = Proxy::new(&conn, DESKTOP, DESKTOP_PATH, SCREENCAST_IFACE)
            .await
            .map_err(|e| e.to_string())?;
        let remote = if devices != 0 {
            Some(
                Proxy::new(&conn, DESKTOP, DESKTOP_PATH, REMOTE_DESKTOP_IFACE)
                    .await
                    .map_err(|e| e.to_string())?,
            )
        } else {
            None
        };
        Ok(Self {
            conn,
            screencast,
            remote,
            session: OwnedObjectPath::default(),
            streams: Vec::new(),
            devices: 0,
            cursor_mode,
            restore_token: None,
            rd_version: 0,
            kde: false,
            mutter: false,
            armed: false,
            notify_tx: OnceLock::new(),
        })
    }

    /// Create the session, select its sources and devices, and start it (`open`).
    async fn start(
        &mut self,
        capture: bool,
        devices: u32,
        restore_token: Option<&str>,
    ) -> Result<(), String> {
        let sc_version: u32 = self
            .screencast
            .get_property("version")
            .await
            .map_err(|e| format!("no ScreenCast portal: {e}"))?;
        let opener = self
            .remote
            .clone()
            .unwrap_or_else(|| self.screencast.clone());
        let rd_version: u32 = match &self.remote {
            Some(rd) => rd
                .get_property("version")
                .await
                .map_err(|e| format!("no RemoteDesktop portal: {e}"))?,
            None => 0,
        };
        self.rd_version = rd_version;
        let mut options = Options::new();
        options.insert(
            "session_handle_token",
            Variant::from(format!("pixelflux{}", std::process::id())),
        );
        let (_, results) = self
            .request("CreateSession", options, async |o| {
                opener.call("CreateSession", &(o,)).await
            })
            .await?;
        let session = results
            .get("session_handle")
            .and_then(|v| match &**v {
                Variant::Str(s) => OwnedObjectPath::try_from(s.as_str()).ok(),
                Variant::ObjectPath(p) => Some(p.clone().into()),
                _ => None,
            })
            .ok_or("CreateSession returned no session handle")?;
        self.session = session;

        // Persistence rides SelectDevices on a remote-desktop session and SelectSources on a
        // plain screencast one; a backend without the option ignores it.
        let persist_on_sources = self.remote.is_none() && sc_version >= 4;
        let persist_on_devices = self.remote.is_some() && rd_version >= 2;
        if capture {
            let available: u32 = self
                .screencast
                .get_property("AvailableCursorModes")
                .await
                .unwrap_or(CURSOR_EMBEDDED);
            if available & self.cursor_mode == 0 {
                self.cursor_mode = [CURSOR_EMBEDDED, CURSOR_HIDDEN, CURSOR_METADATA]
                    .into_iter()
                    .find(|m| available & m != 0)
                    .unwrap_or(CURSOR_EMBEDDED);
            }
            let mut options = Options::new();
            opt_u32(&mut options, "types", SOURCE_MONITOR);
            options.insert("multiple", Variant::Bool(true));
            if sc_version >= 2 {
                opt_u32(&mut options, "cursor_mode", self.cursor_mode);
            }
            if persist_on_sources {
                opt_u32(&mut options, "persist_mode", PERSIST_UNTIL_REVOKED);
                if let Some(t) = restore_token {
                    options.insert("restore_token", Variant::from(t.to_string()));
                }
            }
            let (sc, session) = (self.screencast.clone(), self.session.clone());
            self.request("SelectSources", options, async |o| {
                sc.call("SelectSources", &(&session, o)).await
            })
            .await?;
        }
        if let Some(rd) = self.remote.clone() {
            let available: u32 = rd
                .get_property("AvailableDeviceTypes")
                .await
                .unwrap_or(DEVICE_KEYBOARD | DEVICE_POINTER);
            let mut options = Options::new();
            opt_u32(&mut options, "types", devices & available);
            if persist_on_devices {
                opt_u32(&mut options, "persist_mode", PERSIST_UNTIL_REVOKED);
                if let Some(t) = restore_token {
                    options.insert("restore_token", Variant::from(t.to_string()));
                }
            }
            let session = self.session.clone();
            self.request("SelectDevices", options, async |o| {
                rd.call("SelectDevices", &(&session, o)).await
            })
            .await?;
        }
        let session = self.session.clone();
        let (_, results) = self
            .request("Start", Options::new(), async |o| {
                opener.call("Start", &(&session, "", o)).await
            })
            .await?;
        if let Some(v) = results.get("streams") {
            self.streams = parse_streams(v);
        }
        if let Some(Variant::U32(d)) = results.get("devices").map(|v| &**v) {
            self.devices = *d;
        }
        if let Some(Variant::Str(t)) = results.get("restore_token").map(|v| &**v) {
            self.restore_token = Some(t.to_string());
            save_restore_token(t);
        }
        if capture && self.streams.is_empty() {
            return Err("the portal started no monitor stream".into());
        }
        // The backend has answered Start, so it runs by now; the frontend names none.
        self.kde = self.remote.is_some()
            && name_owned(&self.conn, "org.freedesktop.impl.portal.desktop.kde").await;
        self.mutter =
            self.remote.is_some() && name_owned(&self.conn, "org.gnome.Mutter.RemoteDesktop").await;
        Ok(())
    }

    /// Whether a ScreenCast portal answers on the session bus; the error says which step
    /// did not, since a bus nobody listens on and a portal without a backend read the same.
    pub fn probe() -> Result<(), String> {
        zbus::block_on(async {
            let conn = Connection::session()
                .await
                .map_err(|e| format!("session bus: {e}"))?;
            let screencast = Proxy::new(&conn, DESKTOP, DESKTOP_PATH, SCREENCAST_IFACE)
                .await
                .map_err(|e| e.to_string())?;
            screencast
                .get_property::<u32>("version")
                .await
                .map(|_| ())
                .map_err(|e| format!("no ScreenCast portal: {e}"))
        })
    }

    /// A method whose result arrives as a `Response` on a request object: subscribe to the
    /// request path the handle token names before the call `call` makes with the completed
    /// options, then wait for the answer.
    async fn request(
        &self,
        method: &str,
        mut options: Options<'_>,
        call: impl AsyncFnOnce(&Options<'_>) -> zbus::Result<OwnedObjectPath>,
    ) -> Result<(u32, HashMap<String, OwnedVariant>), String> {
        static COUNTER: AtomicU32 = AtomicU32::new(1);
        let token = format!(
            "pixelflux{}_{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let sender = self
            .conn
            .unique_name()
            .ok_or("bus connection has no unique name")?
            .as_str()
            .to_string();
        let path = request_path(&sender, &token);
        options.insert("handle_token", Variant::from(token.clone()));
        let request = Proxy::new(&self.conn, DESKTOP, path.as_str(), REQUEST_IFACE)
            .await
            .map_err(|e| e.to_string())?;
        let mut responses = request
            .receive_signal("Response")
            .await
            .map_err(|e| e.to_string())?;
        let handle = call(&options).await.map_err(|e| format!("{method}: {e}"))?;
        if handle.as_str() != path {
            // A portal predating handle tokens answers on the path it returned.
            let request = Proxy::new(&self.conn, DESKTOP, handle.as_str(), REQUEST_IFACE)
                .await
                .map_err(|e| e.to_string())?;
            responses = request
                .receive_signal("Response")
                .await
                .map_err(|e| e.to_string())?;
        }
        let msg = next_message(&mut responses)
            .await
            .ok_or_else(|| format!("{method}: the request closed without a response"))?;
        let (code, results): (u32, HashMap<String, OwnedVariant>) = msg
            .body()
            .deserialize()
            .map_err(|e| format!("{method}: {e}"))?;
        match code {
            0 => Ok((code, results)),
            1 => Err(format!("{method}: the user canceled the portal dialog")),
            _ => Err(format!("{method}: the portal refused (response {code})")),
        }
    }

    /// Whether this session can hand out an EIS socket: a RemoteDesktop session (input was
    /// granted) on a backend new enough to carry `ConnectToEIS`.
    pub fn eis_capable(&self) -> bool {
        self.remote.is_some() && self.devices != 0 && self.rd_version >= 2
    }

    /// The EIS socket for this session's seat. A backend that answers hands input over libei
    /// from here on and refuses the `Notify*` methods, so this is called once, at open, and
    /// only when the injector is committed to.
    pub fn connect_to_eis(&self) -> Result<OwnedFd, String> {
        let rd = self.remote.as_ref().ok_or("session drives no input")?;
        let fd: zbus::OwnedFd =
            zbus::block_on(rd.call("ConnectToEIS", &(self.session.clone(), Options::new())))
                .map_err(|e| format!("ConnectToEIS: {e}"))?;
        Ok(fd.into())
    }

    /// The PipeWire connection the streams live on, opened by the portal with the node
    /// permissions of this session.
    pub fn open_pipewire_remote(&self) -> Result<OwnedFd, String> {
        let fd: zbus::OwnedFd = zbus::block_on(self.screencast.call(
            "OpenPipeWireRemote",
            &(self.session.clone(), Options::new()),
        ))
        .map_err(|e| format!("OpenPipeWireRemote: {e}"))?;
        Ok(fd.into())
    }

    /// Queue a `Notify*` call for the input thread, started by the first.
    fn notify<B>(&self, method: &'static str, args: B)
    where
        B: serde::Serialize + zbus::DynamicType + Send + 'static,
    {
        let Some(rd) = &self.remote else { return };
        let tx = self.notify_tx.get_or_init(|| {
            let (tx, rx) = mpsc::channel();
            let rd = rd.clone();
            let _ = std::thread::Builder::new()
                .name("pf-portal-input".into())
                .spawn(move || notify_loop(rd, rx));
            tx
        });
        let _ = tx.send((
            method,
            Box::new(move |rd: &Proxy<'static>| {
                let _ = zbus::block_on(rd.call_method(method, &args));
            }),
        ));
    }

    /// Absolute pointer motion in the logical coordinate space of the stream `node`, the `size`
    /// it announced: the portal frontend refuses a point outside that size.
    pub fn pointer_motion_abs(&self, node: u32, x: f64, y: f64) {
        self.notify(
            MOTION_ABSOLUTE,
            (self.session.clone(), Options::new(), node, x, y),
        );
    }

    pub fn pointer_motion(&self, dx: f64, dy: f64) {
        self.notify(
            "NotifyPointerMotion",
            (self.session.clone(), Options::new(), dx, dy),
        );
    }

    /// Button by evdev code.
    pub fn pointer_button(&self, button: i32, pressed: bool) {
        self.notify(
            "NotifyPointerButton",
            (self.session.clone(), Options::new(), button, pressed as u32),
        );
    }

    /// Smooth scroll by logical pixels, positive down and right as on `wl_pointer`; `finish`
    /// closes the scroll series. KDE's backend negates the vertical axis it is handed and drops
    /// `finish` (xdg-desktop-portal-kde's `requestPointerAxis`, 5.27 through master), where
    /// GNOME's passes both on, so it is handed the axis negated.
    pub fn pointer_axis(&self, dx: f64, dy: f64, finish: bool) {
        let mut options = Options::new();
        options.insert("finish", Variant::Bool(finish));
        let dy = if self.kde { -dy } else { dy };
        self.notify("NotifyPointerAxis", (self.session.clone(), options, dx, dy));
    }

    /// Wheel steps: `axis` 0 vertical, 1 horizontal.
    pub fn pointer_axis_discrete(&self, axis: u32, steps: i32) {
        self.notify(
            "NotifyPointerAxisDiscrete",
            (self.session.clone(), Options::new(), axis, steps),
        );
    }

    pub fn keysym(&self, keysym: i32, pressed: bool) {
        self.notify(
            "NotifyKeyboardKeysym",
            (self.session.clone(), Options::new(), keysym, pressed as u32),
        );
    }

    /// Key by evdev keycode, in the host's own layout.
    pub fn keycode(&self, keycode: i32, pressed: bool) {
        self.notify(
            "NotifyKeyboardKeycode",
            (
                self.session.clone(),
                Options::new(),
                keycode,
                pressed as u32,
            ),
        );
    }
}

impl PortalSession {
    /// Close the session on the portal, if one was created.
    async fn close(&self) {
        if let Ok(session) =
            Proxy::new(&self.conn, DESKTOP, self.session.as_str(), SESSION_IFACE).await
        {
            let _ = session.call_noreply("Close", &()).await;
        }
    }
}

impl Drop for PortalSession {
    fn drop(&mut self) {
        if self.armed {
            zbus::block_on(self.close());
        }
    }
}

/// The input thread: each `Notify*` call once the one before it is answered, an absolute motion
/// with another queued behind it left out; it ends with the session's queue.
fn notify_loop<P>(rd: P, rx: mpsc::Receiver<NotifyCall<P>>) {
    let mut next = rx.recv().ok();
    while let Some((method, call)) = next.take() {
        next = rx.try_recv().ok();
        if method == MOTION_ABSOLUTE && next.as_ref().is_some_and(|(m, _)| *m == MOTION_ABSOLUTE) {
            continue;
        }
        call(&rd);
        if next.is_none() {
            next = rx.recv().ok();
        }
    }
}

/// The request object the portal answers a call from `sender` with `token` on.
fn request_path(sender: &str, token: &str) -> String {
    format!(
        "{DESKTOP_PATH}/request/{}/{token}",
        sender.trim_start_matches(':').replace('.', "_")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::Type as _;
    use zbus::wire::StructureBuilder;
    use zbus::{Array, Dict, Structure};

    fn stream_value(node: u32, props: Vec<(&'static str, Variant<'static>)>) -> Variant<'static> {
        let mut dict = Dict::new(<&str>::SIGNATURE, Variant::SIGNATURE);
        for (k, v) in props {
            dict.append(Variant::from(k), Variant::Variant(Box::new(v)))
                .unwrap();
        }
        let st: Structure = StructureBuilder::new()
            .add_field(node)
            .append_field(Variant::Dict(dict))
            .build()
            .unwrap();
        Variant::Structure(st)
    }

    /// A `Start` response's stream list parses into node ids with the optional geometry, as
    /// KDE (size only) and GNOME (position and size) send it.
    #[test]
    fn streams_parse_with_and_without_geometry() {
        let kde = stream_value(
            113,
            vec![
                ("size", Variant::from((1280i32, 720i32))),
                ("source_type", Variant::U32(1)),
            ],
        );
        let gnome = stream_value(
            42,
            vec![
                ("position", Variant::from((1920i32, 0i32))),
                ("size", Variant::from((1280i32, 720i32))),
            ],
        );
        let mut arr = Array::new(&kde.value_signature().clone());
        arr.append(kde).unwrap();
        arr.append(gnome).unwrap();
        let streams = parse_streams(&Variant::Array(arr));
        assert_eq!(
            streams,
            vec![
                PortalStream {
                    node_id: 113,
                    position: None,
                    size: Some((1280, 720))
                },
                PortalStream {
                    node_id: 42,
                    position: Some((1920, 0)),
                    size: Some((1280, 720))
                },
            ]
        );
    }

    /// The request path follows the handle-token convention: the sender's unique name with
    /// its colon dropped and dots turned to underscores, then the token.
    #[test]
    fn request_path_follows_the_handle_token_convention() {
        assert_eq!(
            request_path(":1.42", "pixelflux7_3"),
            "/org/freedesktop/portal/desktop/request/1_42/pixelflux7_3"
        );
    }

    /// The input thread makes its calls in the order queued, an absolute motion with another
    /// behind it left out, and ends once the session's queue is gone.
    #[test]
    fn notify_calls_keep_their_order_and_runs_of_motion_end_at_their_last() {
        type Seen = std::rc::Rc<std::cell::RefCell<Vec<&'static str>>>;
        let (tx, rx) = mpsc::channel::<NotifyCall<Seen>>();
        for (method, tag) in [
            (MOTION_ABSOLUTE, "motion 1"),
            (MOTION_ABSOLUTE, "motion 2"),
            ("NotifyPointerButton", "press"),
            ("NotifyPointerButton", "release"),
            (MOTION_ABSOLUTE, "motion 3"),
            ("NotifyPointerMotion", "relative 1"),
            ("NotifyPointerMotion", "relative 2"),
            (MOTION_ABSOLUTE, "motion 4"),
        ] {
            let call = move |seen: &Seen| seen.borrow_mut().push(tag);
            tx.send((method, Box::new(call))).unwrap();
        }
        drop(tx);
        let seen = Seen::default();
        notify_loop(seen.clone(), rx);
        assert_eq!(
            *seen.borrow(),
            [
                "motion 2",
                "press",
                "release",
                "motion 3",
                "relative 1",
                "relative 2",
                "motion 4"
            ]
        );
    }
}
