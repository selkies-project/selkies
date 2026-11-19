/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A RemoteDesktop and ScreenCast portal backend that hands each session to Mutter's own D-Bus
//! API, for the host-portal suite.
//!
//! It forwards the way xdg-desktop-portal-gnome does, without its dialog: every request is
//! granted, and a session records Mutter's primary monitor. One difference: Mutter reads
//! absolute pointer and touch positions in the stream's pixels, while the spec and the portal
//! frontend have them in the stream's logical size, so this backend scales them by the
//! monitor's scale. GNOME's passes them on unchanged, which lands them at 1/scale on a scaled
//! monitor; converting them here, as a fixed backend would, leaves the suite checking the client.
//!
//! `--no-eis` refuses `ConnectToEIS`, as a backend without EIS does, so a client falls back to
//! the Notify methods. `--scale N` first sets the primary monitor's scale through Mutter's
//! DisplayConfig (Mutter 46 has no `gdctl`). `--virtual` offers virtual sources too: a session
//! that selects one records a virtual monitor through Mutter's `RecordVirtual`, which Mutter
//! creates for the session and removes when it stops. Each Notify call it forwards goes to stdout with the
//! time, as Mutter stamps its own log; `mutter_portal: ready` once it owns its bus name. The
//! portal frontend finds it through a `.portal` file naming [`BUS_NAME`].
//!
//! ```text
//! mutter-portal [--no-eis] [--scale N] [--virtual]
//! ```

use std::collections::HashMap;
use std::process::ExitCode;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use zbus::message::Type as MessageType;
use zbus::object_server::ObjectServer;
use zbus::{
    Connection, MatchRule, MessageStream, OwnedFd, OwnedObjectPath, OwnedVariant, Variant,
    interface,
};

const BUS_NAME: &str = "org.freedesktop.impl.portal.desktop.mutter_test";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const RD_NAME: &str = "org.gnome.Mutter.RemoteDesktop";
const RD_PATH: &str = "/org/gnome/Mutter/RemoteDesktop";
const SC_NAME: &str = "org.gnome.Mutter.ScreenCast";
const SC_PATH: &str = "/org/gnome/Mutter/ScreenCast";
const RD_SESSION: &str = "org.gnome.Mutter.RemoteDesktop.Session";
const SC_SESSION: &str = "org.gnome.Mutter.ScreenCast.Session";
const SC_STREAM: &str = "org.gnome.Mutter.ScreenCast.Stream";
const DISPLAY_CONFIG: &str = "org.gnome.Mutter.DisplayConfig";
const DISPLAY_CONFIG_PATH: &str = "/org/gnome/Mutter/DisplayConfig";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
/// Mutter's NotifyPointerAxis flag for a finished scroll.
const AXIS_FINISH: u32 = 1;

type Dict = HashMap<String, OwnedVariant>;
type Reply = (u32, Dict);
/// DisplayConfig's names for a monitor: connector, vendor, product, serial.
type MonitorSpec = (String, String, String, String);
/// A monitor's mode: id, size, refresh, preferred and supported scales, properties.
type Mode = (String, i32, i32, f64, f64, Vec<f64>, Dict);
/// A physical monitor: its names, its modes, its properties.
type Monitor = (MonitorSpec, Vec<Mode>, Dict);
/// A logical monitor: position, scale, transform, whether primary, its monitors, properties.
type Logical = (i32, i32, f64, u32, bool, Vec<MonitorSpec>, Dict);
/// What DisplayConfig's GetCurrentState answers.
type CurrentState = (u32, Vec<Monitor>, Vec<Logical>, Dict);
/// A logical monitor as ApplyMonitorsConfig takes it, each monitor by connector and mode.
type LogicalConfig = (i32, i32, f64, u32, bool, Vec<(String, String, Dict)>);

/// The portal's errors, under the name prefix the frontend passes on.
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.freedesktop.portal.Error")]
enum PortalError {
    #[zbus(error)]
    ZBus(zbus::Error),
    NotAllowed(String),
    Failed(String),
}

impl From<zbus::fdo::Error> for PortalError {
    fn from(e: zbus::fdo::Error) -> PortalError {
        PortalError::Failed(e.to_string())
    }
}

/// One portal session and the Mutter session it forwards to.
#[derive(Default, Clone)]
struct Session {
    remote: bool,
    rd_path: String,
    sc_path: String,
    stream_path: String,
    devices: u32,
    cursor_mode: u32,
    scale: f64,
    /// The session selected a virtual source rather than a monitor.
    virtual_src: bool,
}

#[derive(Default)]
struct State {
    sessions: HashMap<String, Session>,
}

type Shared = Arc<Mutex<State>>;

fn session(state: &Shared, handle: &str) -> Result<Session, PortalError> {
    state
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .sessions
        .get(handle)
        .cloned()
        .ok_or_else(|| PortalError::Failed(format!("no session {handle}")))
}

fn update(state: &Shared, handle: &str, change: impl FnOnce(&mut Session)) {
    if let Some(s) = state
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .sessions
        .get_mut(handle)
    {
        change(s);
    }
}

fn variant<T: Into<Variant<'static>>>(value: T) -> OwnedVariant {
    OwnedVariant::try_from(value.into()).unwrap_or_else(|_| OwnedVariant::from(0u32))
}

fn dict_u32(options: &Dict, key: &str) -> Option<u32> {
    options.get(key).and_then(|v| u32::try_from(v).ok())
}

/// One call to Mutter, its reply's body read as `R`.
async fn call<B, R>(
    conn: &Connection,
    name: &str,
    path: &str,
    iface: &str,
    method: &str,
    body: &B,
) -> Result<R, PortalError>
where
    B: serde::Serialize + zbus::DynamicType,
    R: for<'d> zbus::wire::DynamicDeserialize<'d>,
{
    let reply = conn
        .call_method(Some(name), path, Some(iface), method, body)
        .await?;
    Ok(reply.body().deserialize::<R>()?)
}

/// A property of an object of Mutter's.
async fn property(
    conn: &Connection,
    name: &str,
    path: &str,
    iface: &str,
    prop: &str,
) -> Result<OwnedVariant, PortalError> {
    call(conn, name, path, PROPERTIES, "Get", &(iface, prop)).await
}

/// The scale of Mutter's logical monitor at `position`.
async fn monitor_scale(conn: &Connection, position: (i32, i32)) -> f64 {
    let state: Result<CurrentState, _> = call(
        conn,
        DISPLAY_CONFIG,
        DISPLAY_CONFIG_PATH,
        DISPLAY_CONFIG,
        "GetCurrentState",
        &(),
    )
    .await;
    state
        .ok()
        .and_then(|(_, _, logical, _)| {
            logical
                .into_iter()
                .find(|m| (m.0, m.1) == position)
                .map(|m| m.2)
        })
        .unwrap_or(1.0)
}

/// Put the primary monitor alone at `scale` in its current mode, until Mutter exits.
async fn set_scale(conn: &Connection, scale: f64) -> Result<(), PortalError> {
    let (serial, monitors, _, _): CurrentState = call(
        conn,
        DISPLAY_CONFIG,
        DISPLAY_CONFIG_PATH,
        DISPLAY_CONFIG,
        "GetCurrentState",
        &(),
    )
    .await?;
    let ((connector, _, _, _), modes, _) = monitors
        .into_iter()
        .next()
        .ok_or_else(|| PortalError::Failed("no monitor".into()))?;
    let current = modes
        .iter()
        .find(|m| {
            m.6.get("is-current")
                .and_then(|v| bool::try_from(v).ok())
                .unwrap_or(false)
        })
        .map(|m| m.0.clone())
        .unwrap_or_default();
    let logical: Vec<LogicalConfig> = vec![(
        0,
        0,
        scale,
        0,
        true,
        vec![(connector, current, Dict::new())],
    )];
    // Method 1 applies the layout without saving it.
    let _: () = call(
        conn,
        DISPLAY_CONFIG,
        DISPLAY_CONFIG_PATH,
        DISPLAY_CONFIG,
        "ApplyMonitorsConfig",
        &(serial, 1u32, logical, Dict::new()),
    )
    .await?;
    Ok(())
}

/// The portal Session object a frontend's handle names: closing it stops Mutter's session.
struct SessionObject {
    conn: Connection,
    state: Shared,
}

#[interface(name = "org.freedesktop.impl.portal.Session")]
impl SessionObject {
    async fn close(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(object_server)] server: &ObjectServer,
    ) {
        let Some(path) = header.path().map(ToString::to_string) else {
            return;
        };
        let s = self
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .sessions
            .remove(&path);
        if let Some(s) = s {
            let stopped: Result<(), _> = if s.remote {
                call(&self.conn, RD_NAME, &s.rd_path, RD_SESSION, "Stop", &()).await
            } else {
                call(&self.conn, SC_NAME, &s.sc_path, SC_SESSION, "Stop", &()).await
            };
            drop(stopped);
        }
        let _ = server.remove::<SessionObject, _>(path.as_str()).await;
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        1
    }
}

/// What both interfaces share: the connection, the sessions, whether EIS is offered, and
/// whether virtual sources are.
#[derive(Clone)]
struct Backend {
    conn: Connection,
    state: Shared,
    eis: bool,
    virtual_sources: bool,
}

impl Backend {
    async fn create_session(
        &self,
        handle: &str,
        remote: bool,
        server: &ObjectServer,
    ) -> Result<Reply, PortalError> {
        let mut s = Session {
            remote,
            devices: 3,
            scale: 1.0,
            ..Session::default()
        };
        let mut props = Dict::new();
        if remote {
            let (rd_path,): (OwnedObjectPath,) =
                call(&self.conn, RD_NAME, RD_PATH, RD_NAME, "CreateSession", &()).await?;
            s.rd_path = rd_path.to_string();
            let id = property(&self.conn, RD_NAME, &s.rd_path, RD_SESSION, "SessionId").await?;
            props.insert("remote-desktop-session-id".into(), id);
        }
        let (sc_path,): (OwnedObjectPath,) = call(
            &self.conn,
            SC_NAME,
            SC_PATH,
            SC_NAME,
            "CreateSession",
            &(props,),
        )
        .await?;
        s.sc_path = sc_path.to_string();
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .sessions
            .insert(handle.to_owned(), s);
        server
            .at(
                handle,
                SessionObject {
                    conn: self.conn.clone(),
                    state: Arc::clone(&self.state),
                },
            )
            .await?;
        Ok((0, Dict::new()))
    }

    /// Record the primary monitor, or a virtual one where the session selected that, start,
    /// and answer once Mutter names the stream's node.
    async fn start(&self, handle: &str) -> Result<Reply, PortalError> {
        let s = session(&self.state, handle)?;
        let mut options = Dict::new();
        options.insert("cursor-mode".into(), variant(s.cursor_mode));
        let (stream,): (OwnedObjectPath,) = if s.virtual_src {
            call(
                &self.conn,
                SC_NAME,
                &s.sc_path,
                SC_SESSION,
                "RecordVirtual",
                &(options,),
            )
            .await?
        } else {
            call(
                &self.conn,
                SC_NAME,
                &s.sc_path,
                SC_SESSION,
                "RecordMonitor",
                &("", options),
            )
            .await?
        };
        let stream_path = stream.to_string();
        update(&self.state, handle, |x| {
            x.stream_path.clone_from(&stream_path)
        });
        let rule = MatchRule::builder()
            .msg_type(MessageType::Signal)
            .interface(SC_STREAM)
            .member("PipeWireStreamAdded")
            .path(stream_path.as_str())
            .build()?;
        let mut added = MessageStream::for_match_rule(rule, &self.conn, None).await?;
        if s.remote {
            let _: () = call(&self.conn, RD_NAME, &s.rd_path, RD_SESSION, "Start", &()).await?;
        } else {
            let _: () = call(&self.conn, SC_NAME, &s.sc_path, SC_SESSION, "Start", &()).await?;
        }
        let message = added
            .next()
            .await
            .ok_or_else(|| PortalError::Failed("no PipeWire stream".into()))??;
        let (node_id,): (u32,) = message.body().deserialize()?;
        let parameters =
            property(&self.conn, SC_NAME, &stream_path, SC_STREAM, "Parameters").await?;
        let p: Dict = Dict::try_from(parameters).unwrap_or_default();
        let pair = |key: &str| {
            p.get(key)
                .and_then(|v| v.try_clone().ok())
                .and_then(|v| <(i32, i32)>::try_from(v).ok())
        };
        let mut props = Dict::new();
        props.insert("id".into(), variant("0"));
        props.insert(
            "source_type".into(),
            variant(if s.virtual_src { 4u32 } else { 1u32 }),
        );
        let position = pair("position");
        if let Some(at) = position {
            props.insert("position".into(), variant(at));
        }
        if let Some(size) = pair("size") {
            props.insert("size".into(), variant(size));
        }
        if let Some(id) = p
            .get("mapping-id")
            .and_then(|v| v.try_clone().ok())
            .and_then(|v| String::try_from(v).ok())
        {
            props.insert("mapping_id".into(), variant(id));
        }
        let scale = monitor_scale(&self.conn, position.unwrap_or((0, 0))).await;
        update(&self.state, handle, |x| x.scale = scale);
        println!(
            "mutter_portal: stream {node_id}: {} cursor mode {}, Mutter's parameters {p:?}, scale {scale}",
            if s.virtual_src {
                "virtual monitor"
            } else {
                "monitor"
            },
            s.cursor_mode
        );
        let mut results = Dict::new();
        results.insert("streams".into(), variant(vec![(node_id, props)]));
        if s.remote {
            results.insert("devices".into(), variant(s.devices));
        }
        Ok((0, results))
    }

    /// Forward one Notify call to Mutter's session, stamped as Mutter stamps its log.
    async fn notify<B: serde::Serialize + zbus::DynamicType>(
        &self,
        handle: &str,
        method: &str,
        body: &B,
        shown: &str,
    ) -> Result<(), PortalError> {
        let s = session(&self.state, handle)?;
        let sent: Result<(), PortalError> =
            call(&self.conn, RD_NAME, &s.rd_path, RD_SESSION, method, body).await;
        if let Err(e) = sent {
            eprintln!("mutter_portal: {method} failed: {e:?}");
            return Err(e);
        }
        println!("mutter_portal: {} {method} {shown}", clock());
        Ok(())
    }
}

/// The local time of day as `HH:MM:SS.mmm`.
fn clock() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = libc::time_t::try_from(now.as_secs()).unwrap_or(0);
    // SAFETY: localtime_r fills the tm it is handed from a valid time_t.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&raw const secs, &raw mut tm) };
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        now.subsec_millis()
    )
}

struct ScreenCast(Backend);

#[interface(name = "org.freedesktop.impl.portal.ScreenCast")]
impl ScreenCast {
    async fn create_session(
        &self,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        _app_id: String,
        _options: Dict,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> Result<Reply, PortalError> {
        self.0
            .create_session(session_handle.as_str(), false, server)
            .await
    }

    async fn select_sources(
        &self,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        _app_id: String,
        options: Dict,
    ) -> Result<Reply, PortalError> {
        if self.0.virtual_sources && dict_u32(&options, "types").is_some_and(|t| t & 4 != 0) {
            update(&self.0.state, session_handle.as_str(), |s| {
                s.virtual_src = true
            });
        }
        // The portal's cursor mode bits (hidden, embedded, metadata) as Mutter's cursor-mode values.
        if let Some(mode) = dict_u32(&options, "cursor_mode").and_then(|m| match m {
            1 => Some(0),
            2 => Some(1),
            4 => Some(2),
            _ => None,
        }) {
            update(&self.0.state, session_handle.as_str(), |s| {
                s.cursor_mode = mode
            });
        }
        Ok((0, Dict::new()))
    }

    async fn start(
        &self,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        _app_id: String,
        _parent: String,
        _options: Dict,
    ) -> Result<Reply, PortalError> {
        self.0.start(session_handle.as_str()).await
    }

    /// Monitors, and virtual monitors under `--virtual`.
    #[zbus(property, name = "AvailableSourceTypes")]
    fn available_source_types(&self) -> u32 {
        if self.0.virtual_sources { 5 } else { 1 }
    }

    /// Hidden, embedded, and metadata cursors.
    #[zbus(property, name = "AvailableCursorModes")]
    fn available_cursor_modes(&self) -> u32 {
        7
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        5
    }
}

struct RemoteDesktop(Backend);

#[interface(name = "org.freedesktop.impl.portal.RemoteDesktop")]
impl RemoteDesktop {
    async fn create_session(
        &self,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        _app_id: String,
        _options: Dict,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> Result<Reply, PortalError> {
        self.0
            .create_session(session_handle.as_str(), true, server)
            .await
    }

    async fn select_devices(
        &self,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        _app_id: String,
        options: Dict,
    ) -> Result<Reply, PortalError> {
        let types = dict_u32(&options, "types");
        update(&self.0.state, session_handle.as_str(), |s| {
            let wanted = types.unwrap_or(s.devices) & 3;
            s.devices = if wanted == 0 { 3 } else { wanted };
        });
        Ok((0, Dict::new()))
    }

    async fn start(
        &self,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        _app_id: String,
        _parent: String,
        _options: Dict,
    ) -> Result<Reply, PortalError> {
        self.0.start(session_handle.as_str()).await
    }

    async fn notify_pointer_motion(
        &self,
        session_handle: OwnedObjectPath,
        _options: Dict,
        dx: f64,
        dy: f64,
    ) -> Result<(), PortalError> {
        self.0
            .notify(
                session_handle.as_str(),
                "NotifyPointerMotionRelative",
                &(dx, dy),
                &format!("({dx}, {dy})"),
            )
            .await
    }

    async fn notify_pointer_motion_absolute(
        &self,
        session_handle: OwnedObjectPath,
        _options: Dict,
        stream: u32,
        x: f64,
        y: f64,
    ) -> Result<(), PortalError> {
        let s = session(&self.0.state, session_handle.as_str())?;
        self.0
            .notify(
                session_handle.as_str(),
                "NotifyPointerMotionAbsolute",
                &(s.stream_path.as_str(), x * s.scale, y * s.scale),
                &format!("({stream}, {x}, {y})"),
            )
            .await
    }

    async fn notify_pointer_button(
        &self,
        session_handle: OwnedObjectPath,
        _options: Dict,
        button: i32,
        state: u32,
    ) -> Result<(), PortalError> {
        self.0
            .notify(
                session_handle.as_str(),
                "NotifyPointerButton",
                &(button, state != 0),
                &format!("({button}, {state})"),
            )
            .await
    }

    async fn notify_pointer_axis(
        &self,
        session_handle: OwnedObjectPath,
        options: Dict,
        dx: f64,
        dy: f64,
    ) -> Result<(), PortalError> {
        let finish = options
            .get("finish")
            .and_then(|v| bool::try_from(v).ok())
            .unwrap_or(false);
        self.0
            .notify(
                session_handle.as_str(),
                "NotifyPointerAxis",
                &(dx, dy, if finish { AXIS_FINISH } else { 0 }),
                &format!("({dx}, {dy})"),
            )
            .await
    }

    async fn notify_pointer_axis_discrete(
        &self,
        session_handle: OwnedObjectPath,
        _options: Dict,
        axis: u32,
        steps: i32,
    ) -> Result<(), PortalError> {
        self.0
            .notify(
                session_handle.as_str(),
                "NotifyPointerAxisDiscrete",
                &(axis, steps),
                &format!("({axis}, {steps})"),
            )
            .await
    }

    async fn notify_keyboard_keycode(
        &self,
        session_handle: OwnedObjectPath,
        _options: Dict,
        keycode: i32,
        state: u32,
    ) -> Result<(), PortalError> {
        let code = u32::try_from(keycode).unwrap_or(0);
        self.0
            .notify(
                session_handle.as_str(),
                "NotifyKeyboardKeycode",
                &(code, state != 0),
                &format!("({keycode}, {state})"),
            )
            .await
    }

    async fn notify_keyboard_keysym(
        &self,
        session_handle: OwnedObjectPath,
        _options: Dict,
        keysym: i32,
        state: u32,
    ) -> Result<(), PortalError> {
        let sym = u32::try_from(keysym).unwrap_or(0);
        self.0
            .notify(
                session_handle.as_str(),
                "NotifyKeyboardKeysym",
                &(sym, state != 0),
                &format!("({keysym}, {state})"),
            )
            .await
    }

    async fn notify_touch_down(
        &self,
        session_handle: OwnedObjectPath,
        _options: Dict,
        stream: u32,
        slot: u32,
        x: f64,
        y: f64,
    ) -> Result<(), PortalError> {
        let s = session(&self.0.state, session_handle.as_str())?;
        self.0
            .notify(
                session_handle.as_str(),
                "NotifyTouchDown",
                &(s.stream_path.as_str(), slot, x * s.scale, y * s.scale),
                &format!("({stream}, {slot}, {x}, {y})"),
            )
            .await
    }

    async fn notify_touch_motion(
        &self,
        session_handle: OwnedObjectPath,
        _options: Dict,
        stream: u32,
        slot: u32,
        x: f64,
        y: f64,
    ) -> Result<(), PortalError> {
        let s = session(&self.0.state, session_handle.as_str())?;
        self.0
            .notify(
                session_handle.as_str(),
                "NotifyTouchMotion",
                &(s.stream_path.as_str(), slot, x * s.scale, y * s.scale),
                &format!("({stream}, {slot}, {x}, {y})"),
            )
            .await
    }

    async fn notify_touch_up(
        &self,
        session_handle: OwnedObjectPath,
        _options: Dict,
        slot: u32,
    ) -> Result<(), PortalError> {
        self.0
            .notify(
                session_handle.as_str(),
                "NotifyTouchUp",
                &(slot,),
                &format!("({slot},)"),
            )
            .await
    }

    #[zbus(name = "ConnectToEIS")]
    async fn connect_to_eis(
        &self,
        session_handle: OwnedObjectPath,
        _app_id: String,
        _options: Dict,
    ) -> Result<OwnedFd, PortalError> {
        if !self.0.eis {
            return Err(PortalError::NotAllowed("this backend offers no EIS".into()));
        }
        let s = session(&self.0.state, session_handle.as_str())?;
        let mut options = Dict::new();
        options.insert("device-types".into(), variant(s.devices));
        let (fd,): (OwnedFd,) = call(
            &self.0.conn,
            RD_NAME,
            &s.rd_path,
            RD_SESSION,
            "ConnectToEIS",
            &(options,),
        )
        .await?;
        Ok(fd)
    }

    /// Keyboard and pointer.
    #[zbus(property, name = "AvailableDeviceTypes")]
    fn available_device_types(&self) -> u32 {
        3
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        2
    }
}

/// Wait until Mutter owns each of `names`: it opens its Wayland socket before it takes them on
/// the bus, by seconds where a session service it asks first times out.
async fn wait_for_names(
    conn: &Connection,
    names: &[&str],
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    for name in names {
        loop {
            let owned: Result<(bool,), PortalError> = call(
                conn,
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "NameHasOwner",
                &(*name,),
            )
            .await;
            if matches!(owned, Ok((true,))) {
                break;
            }
            if Instant::now() > deadline {
                return Err(format!("{name} never appeared on the bus"));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    Ok(())
}

async fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let eis = !args.iter().any(|a| a == "--no-eis");
    let virtual_sources = args.iter().any(|a| a == "--virtual");
    let scale = args
        .iter()
        .position(|a| a == "--scale")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<f64>().ok());
    let conn = Connection::session().await.map_err(|e| e.to_string())?;
    wait_for_names(
        &conn,
        &[RD_NAME, SC_NAME, DISPLAY_CONFIG],
        Duration::from_secs(30),
    )
    .await?;
    if let Some(scale) = scale {
        set_scale(&conn, scale)
            .await
            .map_err(|e| format!("{e:?}"))?;
    }
    let backend = Backend {
        conn: conn.clone(),
        state: Shared::default(),
        eis,
        virtual_sources,
    };
    conn.object_server()
        .at(PORTAL_PATH, ScreenCast(backend.clone()))
        .await
        .map_err(|e| e.to_string())?;
    conn.object_server()
        .at(PORTAL_PATH, RemoteDesktop(backend))
        .await
        .map_err(|e| e.to_string())?;
    conn.request_name(BUS_NAME)
        .await
        .map_err(|e| e.to_string())?;
    println!("mutter_portal: ready");
    std::future::pending::<()>().await;
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mutter-portal: {e}");
            ExitCode::FAILURE
        }
    }
}
