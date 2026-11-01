/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The retained X connection and every blocking RandR operation on it.
//!
//! RandR user modes are owned by the connection that created them and die with it (the
//! screen falls back to a built-in mode), so the connection stays open for the process
//! lifetime and retains its resources past disconnect (`RetainTemporary`). Retention is
//! temporary: a retained client record holds one of the server's client slots until
//! something issues `KillClient(AllTemporary)`, and a server whose Selkies is restarted
//! many times would otherwise run out of slots. So the first connection of a process reaps
//! the records its predecessors left, and when that takes the live mode with it (the
//! screen reverts), it recreates that mode at once: a brief revert on a restart, no slot
//! ever held by a finished process.
//!
//! The connection carries a blocking timeout (`TimedStream`): an alive-but-unresponsive X
//! server (driver hang, a foreign client's server grab) would otherwise block these
//! helpers forever -- in the handshake, then in any reply wait -- while they hold the
//! lock, and every retry would park another worker behind it. The bound makes both fail
//! with a connection error, so the helpers drop this connection and fall back to their
//! subprocess paths.
//!
//! Every helper runs under `with_x11`, which connects on demand, hands the closure the
//! connection, and drops the connection on any failure other than an X protocol error
//! (`DisplayError::is_protocol`). Blocking work runs through `on_x11`, on a blocking
//! thread of the runtime, never on its workers.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, IoSlice};
use std::os::fd::AsFd;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use log::{debug, info, warn};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::randr::{self, ConnectionExt as _, ModeInfo, SetConfig};
use x11rb::protocol::res::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ClientMessageEvent, CloseDown, ConnectionExt as _, EventMask, Gravity, Kill,
    PropMode, Window,
};
use x11rb::reexports::x11rb_protocol::{parse_display, xauth};
use x11rb::rust_connection::{DefaultStream, PollMode, RustConnection, Stream};
use x11rb::utils::RawFdContainer;

use super::geometry::window_moves;
use super::modes::{self, ModePool, ModeServer, ModeTimings, mode_at, target_refresh};
use super::{DisplayError, Layouts, Rect};
use crate::core::PRIMARY_DISPLAY;

/// How long a reply or the handshake may take before the connection counts as dead.
const BLOCKING_TIMEOUT: Duration = Duration::from_secs(15);

/// The root window property keeping the configured refresh, in millihertz, for every later
/// process on the same server: by then the display may run at a rate an earlier stream
/// raised it to, which is no configuration.
const CONFIGURED_REFRESH_PROP: &str = "_SELKIES_CONFIGURED_REFRESH";

/// The output property a server offers on an output a client may plug in and out; its
/// presence is how the capability is detected.
const CONNECTED_PROP: &str = "Connected";

/// Window types a layout leaves where they are: the desktop window covers the root whatever
/// the layout, and a panel places itself on its monitor's edge.
const SELF_PLACING_WINDOW_TYPES: [&str; 2] =
    ["_NET_WM_WINDOW_TYPE_DESKTOP", "_NET_WM_WINDOW_TYPE_DOCK"];

/// How long a window manager is given to put the windows where they were asked to go, read
/// back and asked again meanwhile.
const MANAGER_SETTLE: Duration = Duration::from_millis(1500);

/// How far a client may sit from where it was asked to go and count as there.
const MOVE_SLACK: i32 = 2;

/// How long a desktop is given to size its background window to a new arrangement, watched
/// for one left away from the origin.
const DESKTOP_SETTLE: Duration = Duration::from_secs(3);

/// The pause between two reads of what the manager did with a request.
const MANAGER_POLL: Duration = Duration::from_millis(150);

/// `_NET_MOVERESIZE_WINDOW` flags: static gravity, x and y present, from a pager.
fn move_flags() -> u32 {
    u32::from(Gravity::STATIC) | (1 << 8) | (1 << 9) | (2 << 12)
}

/// The DPI the last `set_dpi` applied; resizes size the pane in millimeters from it.
static APPLIED_DPI: Mutex<Option<i32>> = Mutex::new(None);

/// The density the last `set_dpi` gave the desktop; `None` before the first.
pub fn applied_dpi() -> Option<i32> {
    *APPLIED_DPI.lock().unwrap_or_else(|e| e.into_inner())
}

/// Record the density the desktop was given.
pub(crate) fn set_applied_dpi(dpi: i32) {
    *APPLIED_DPI.lock().unwrap_or_else(|e| e.into_inner()) = Some(dpi);
}

/// The physical size RandR is told for `pixels` at the applied density (96 when none).
fn millimeters(pixels: i32) -> u32 {
    let dpi = applied_dpi().unwrap_or(96) as f64;
    ((pixels as f64 * 25.4 / dpi).round() as u32).max(1)
}

/// The pluggable output each secondary display holds, so a display that stays from one
/// layout to the next keeps its output.
static OUTPUT_OF: Mutex<BTreeMap<String, u32>> = Mutex::new(BTreeMap::new());

fn output_of() -> std::sync::MutexGuard<'static, BTreeMap<String, u32>> {
    OUTPUT_OF.lock().unwrap_or_else(|e| e.into_inner())
}

/// A `DefaultStream` whose waits give up after `BLOCKING_TIMEOUT`, so a hung server fails
/// the request instead of parking the caller forever.
struct TimedStream {
    inner: DefaultStream,
    timeout: Duration,
}

impl Stream for TimedStream {
    fn poll(&self, mode: PollMode) -> io::Result<()> {
        let mut flags = PollFlags::empty();
        if mode.readable() {
            flags |= PollFlags::POLLIN;
        }
        if mode.writable() {
            flags |= PollFlags::POLLOUT;
        }
        let timeout = PollTimeout::try_from(self.timeout).unwrap_or(PollTimeout::NONE);
        loop {
            let fd = self.inner.as_fd();
            let mut fds = [PollFd::new(fd, flags)];
            match poll(&mut fds, timeout) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("X server did not answer within {}s", self.timeout.as_secs()),
                    ));
                }
                Ok(_) => return Ok(()),
                Err(nix::errno::Errno::EINTR) => {}
                Err(e) => return Err(io::Error::from(e)),
            }
        }
    }

    fn read(&self, buf: &mut [u8], fd_storage: &mut Vec<RawFdContainer>) -> io::Result<usize> {
        self.inner.read(buf, fd_storage)
    }

    fn write(&self, buf: &[u8], fds: &mut Vec<RawFdContainer>) -> io::Result<usize> {
        self.inner.write(buf, fds)
    }

    fn write_vectored(
        &self,
        bufs: &[IoSlice<'_>],
        fds: &mut Vec<RawFdContainer>,
    ) -> io::Result<usize> {
        self.inner.write_vectored(bufs, fds)
    }
}

/// A manager's client window with its rectangle in root coordinates: `(window, x, y, w, h)`.
pub type WindowRect = (Window, i32, i32, i32, i32);

/// One connection to the X server named by `DISPLAY`, with the atoms it interned.
pub struct X11 {
    conn: RustConnection<TimedStream>,
    /// The default screen's root window.
    pub root: Window,
    atoms: HashMap<String, Atom>,
}

/// What `connected_output_state` reads of the first connected output.
pub struct OutputState {
    pub res: randr::GetScreenResourcesReply,
    pub out_id: u32,
    pub oi: randr::GetOutputInfoReply,
    /// Every mode of the screen by id and name, and the output's own.
    pub pool: ModePool,
}

/// What `query_randr` answers: the root's size, the output's `WxH` mode names sorted, and
/// the output's name (`None` on a server with no connected output).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RandrQuery {
    pub current: (i32, i32),
    pub resolutions: Vec<String>,
    pub output_name: Option<String>,
}

type Result<T> = std::result::Result<T, DisplayError>;

impl X11 {
    /// Connect to the display `DISPLAY` names, with the blocking timeout, keeping the
    /// connection's resources past its close.
    fn connect(timeout: Duration) -> Result<X11> {
        let parsed = parse_display::parse_display(None)
            .map_err(|e| DisplayError::Connection(format!("DISPLAY: {e}")))?;
        let screen_index = parsed.screen as usize;
        let mut last = None;
        for addr in parsed.connect_instruction() {
            match DefaultStream::connect(&addr) {
                Ok((stream, (family, address))) => {
                    let (auth_name, auth_data) = xauth::get_auth(family, &address, parsed.display)
                        .unwrap_or(None)
                        .unwrap_or_default();
                    let stream = TimedStream {
                        inner: stream,
                        timeout,
                    };
                    let conn = RustConnection::connect_to_stream_with_auth_info(
                        stream,
                        screen_index,
                        auth_name,
                        auth_data,
                    )?;
                    let root = conn
                        .setup()
                        .roots
                        .get(screen_index)
                        .ok_or_else(|| DisplayError::Connection("no such screen".into()))?
                        .root;
                    let x = X11 {
                        conn,
                        root,
                        atoms: HashMap::new(),
                    };
                    x.conn
                        .set_close_down_mode(CloseDown::RETAIN_TEMPORARY)?
                        .check()?;
                    return Ok(x);
                }
                Err(e) => last = Some(e),
            }
        }
        Err(DisplayError::Connection(
            last.map(|e| e.to_string())
                .unwrap_or_else(|| "no address to connect to".into()),
        ))
    }

    /// Intern `name`, cached.
    pub fn atom(&mut self, name: &str) -> Result<Atom> {
        if let Some(a) = self.atoms.get(name) {
            return Ok(*a);
        }
        let atom = self.conn.intern_atom(false, name.as_bytes())?.reply()?.atom;
        self.atoms.insert(name.to_string(), atom);
        Ok(atom)
    }

    /// The name of `atom`.
    fn atom_name(&self, atom: Atom) -> Result<String> {
        Ok(String::from_utf8_lossy(&self.conn.get_atom_name(atom)?.reply()?.name).into_owned())
    }

    /// Round-trip to the server, so every queued request has been carried out.
    pub fn sync(&self) -> Result<()> {
        self.conn.get_input_focus()?.reply()?;
        Ok(())
    }

    /// Flush queued requests to the server.
    pub fn flush(&self) -> Result<()> {
        self.conn.flush()?;
        Ok(())
    }

    /// The root window's size.
    pub fn root_size(&self) -> Result<(i32, i32)> {
        let g = self.conn.get_geometry(self.root)?.reply()?;
        Ok((g.width as i32, g.height as i32))
    }

    /// A 32-bit property of `win`, empty where it is unset.
    fn prop32(&self, win: Window, property: Atom, kind: impl Into<Atom>) -> Result<Vec<u32>> {
        let reply = self
            .conn
            .get_property(false, win, property, kind, 0, u32::MAX)?
            .reply()?;
        Ok(reply.value32().map(|v| v.collect()).unwrap_or_default())
    }

    /// Where `win`'s origin sits in root coordinates, and its size.
    fn window_rect(&self, win: Window) -> Result<Rect> {
        let geom = self.conn.get_geometry(win)?.reply()?;
        let at = self
            .conn
            .translate_coordinates(win, self.root, 0, 0)?
            .reply()?;
        Ok((
            at.dst_x as i32,
            at.dst_y as i32,
            geom.width as i32,
            geom.height as i32,
        ))
    }

    /// Send a client message to the window manager about `win`.
    fn send_to_manager(&mut self, win: Window, kind: &str, data: [u32; 5]) -> Result<()> {
        let atom = self.atom(kind)?;
        let event = ClientMessageEvent::new(32, win, atom, data);
        self.conn.send_event(
            false,
            self.root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            event,
        )?;
        Ok(())
    }

    /// Run `f` under a server grab, so the window manager acts on the finished arrangement
    /// only. The ungrab is flushed, not just queued: an X error aborting the sequence would
    /// leave an unsent ungrab and every other X client wedged until this process exits.
    pub(crate) fn grabbed<T>(&mut self, f: impl FnOnce(&mut X11) -> Result<T>) -> Result<T> {
        self.conn.grab_server()?.check()?;
        let result = f(self);
        if let Ok(cookie) = self.conn.ungrab_server() {
            let _ = cookie.check();
        }
        let _ = self.conn.flush();
        result
    }

    /// The names of `res`'s modes by id.
    fn mode_names(res: &randr::GetScreenResourcesReply) -> Vec<(u32, String)> {
        let mut pos = 0usize;
        res.modes
            .iter()
            .map(|m| {
                let end = (pos + m.name_len as usize).min(res.names.len());
                let name = String::from_utf8_lossy(&res.names[pos.min(end)..end]).into_owned();
                pos = end;
                (m.id, name)
            })
            .collect()
    }

    /// The screen's modes as the selection reads them.
    fn mode_pool(res: &randr::GetScreenResourcesReply, oi: &randr::GetOutputInfoReply) -> ModePool {
        ModePool {
            modes: res
                .modes
                .iter()
                .map(|m| modes::Mode {
                    id: m.id,
                    width: m.width as u32,
                    height: m.height as u32,
                    dot_clock: m.dot_clock as u64,
                    h_total: m.htotal as u32,
                    v_total: m.vtotal as u32,
                    flags: u32::from(m.mode_flags),
                })
                .collect(),
            names: Self::mode_names(res),
            on_output: oi.modes.clone(),
        }
    }

    fn output_info(&self, out: u32, ts: u32) -> Result<randr::GetOutputInfoReply> {
        Ok(self.conn.randr_get_output_info(out, ts)?.reply()?)
    }

    fn crtc_info(&self, crtc: u32, ts: u32) -> Result<randr::GetCrtcInfoReply> {
        Ok(self.conn.randr_get_crtc_info(crtc, ts)?.reply()?)
    }

    /// Locate the first connected RandR output; `DisplayError::NoOutput` where none is.
    pub fn connected_output_state(&self) -> Result<OutputState> {
        let res = self.conn.randr_get_screen_resources(self.root)?.reply()?;
        for &out_id in &res.outputs {
            let oi = self.output_info(out_id, res.config_timestamp)?;
            if oi.connection == randr::Connection::CONNECTED {
                let pool = Self::mode_pool(&res, &oi);
                return Ok(OutputState {
                    res,
                    out_id,
                    oi,
                    pool,
                });
            }
        }
        Err(DisplayError::NoOutput)
    }

    /// The first connected RandR output, or `None` where the server has none.
    ///
    /// Modes belong to an output, so the mode paths need one and fail without it. Logical
    /// monitors do not: a server whose driver reports no display device -- a GPU with no
    /// display engine -- still carries a framebuffer to define them over, and they are the
    /// only screens the toolkits will find there.
    pub fn first_connected_output(&self) -> Result<Option<u32>> {
        match self.connected_output_state() {
            Ok(state) => Ok(Some(state.out_id)),
            Err(DisplayError::NoOutput) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Learn the configured refresh once per process: from the root window where an earlier
    /// process on this server recorded it, else from the mode the display shows before
    /// anything has changed it, then recorded there.
    fn note_configured_refresh(&mut self) {
        if modes::configured_refresh().is_some() {
            return;
        }
        let mut read = || -> Result<()> {
            let atom = self.atom(CONFIGURED_REFRESH_PROP)?;
            let recorded = self.prop32(self.root, atom, AtomEnum::CARDINAL)?;
            if let Some(&mhz) = recorded.first()
                && mhz > 0
            {
                modes::note_configured_refresh(mhz as f64 / 1000.0);
                return Ok(());
            }
            let state = self.connected_output_state()?;
            if state.oi.crtc == 0 {
                return Ok(());
            }
            let mode = self
                .crtc_info(state.oi.crtc, state.res.config_timestamp)?
                .mode;
            let rate = state.pool.mode(mode).map(|m| m.refresh()).unwrap_or(0.0);
            if rate > 0.0 {
                modes::note_configured_refresh(rate);
                let value = ((rate * 1000.0).round() as u32).to_ne_bytes();
                self.conn
                    .change_property(
                        PropMode::REPLACE,
                        self.root,
                        atom,
                        AtomEnum::CARDINAL,
                        32,
                        1,
                        &value,
                    )?
                    .check()?;
            }
            Ok(())
        };
        if let Err(e) = read() {
            debug!(target: "display", "Configured refresh not read: {e}");
        }
    }

    /// Release the temporarily retained resources of earlier processes and restore the
    /// screen size if the reap reverted it.
    fn reap_retained_predecessors(&mut self) {
        let reap = || -> Result<((i32, i32), (i32, i32))> {
            let before = self.root_size()?;
            self.conn.kill_client(Kill::ALL_TEMPORARY)?.check()?;
            let after = self.root_size()?;
            Ok((before, after))
        };
        let (before, after) = match reap() {
            Ok(sizes) => sizes,
            Err(e) => {
                debug!(target: "display", "Retained X client reap skipped: {e}");
                return;
            }
        };
        if after == before {
            return;
        }
        match self.resize_on_display(before.0, before.1, None, None) {
            Ok((w, h, _)) => info!(
                target: "display",
                "Restored the {}x{} screen mode a finished process had left retained (realized {w}x{h}).",
                before.0, before.1
            ),
            Err(e) => warn!(
                target: "display",
                "Screen reverted to {}x{} while reaping retained X clients and could not be restored: {e}",
                after.0, after.1
            ),
        }
    }

    /// Blocking RandR query: the root's size, the first connected output's `WxH` mode names
    /// sorted, and its name; a server with no connected output (a GPU without a display
    /// engine, a driver told to use none) answers the root alone.
    pub fn query_randr(&self) -> Result<RandrQuery> {
        let current = self.root_size()?;
        let state = match self.connected_output_state() {
            Ok(state) => state,
            Err(DisplayError::NoOutput) => {
                return Ok(RandrQuery {
                    current,
                    resolutions: Vec::new(),
                    output_name: None,
                });
            }
            Err(e) => return Err(e),
        };
        let is_wh = |name: &str| {
            name.split_once('x').is_some_and(|(w, h)| {
                !w.is_empty()
                    && !h.is_empty()
                    && w.bytes().all(|b| b.is_ascii_digit())
                    && h.bytes().all(|b| b.is_ascii_digit())
            })
        };
        let mut resolutions: Vec<String> = state
            .oi
            .modes
            .iter()
            .map(|id| state.pool.name_of(*id).to_string())
            .filter(|n| is_wh(n))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        resolutions.sort();
        Ok(RandrQuery {
            current,
            resolutions,
            output_name: Some(String::from_utf8_lossy(&state.oi.name).into_owned()),
        })
    }

    /// Ensure a `w_req` by `h_req` mode is attached to the connected output at the refresh
    /// `target_refresh` makes of `refresh`, without a CRTC or screen change.
    pub fn ensure_mode(&mut self, w_req: i32, h_req: i32, refresh: Option<f64>) -> Result<()> {
        if w_req <= 0 || h_req <= 0 {
            return Err(DisplayError::Invalid(format!(
                "invalid resolution '{w_req}x{h_req}'"
            )));
        }
        let mut state = self.connected_output_state()?;
        let w_cell = (w_req as u32).div_ceil(8) * 8;
        let (mode_id, _) = mode_at(
            self,
            &mut state.pool,
            state.out_id,
            w_cell,
            h_req as u32,
            target_refresh(refresh),
            &format!("{w_cell}x{h_req}"),
        )?;
        self.sync()?;
        let after = self.connected_output_state()?;
        if !after.oi.modes.contains(&mode_id) {
            return Err(DisplayError::Refused(format!(
                "mode '{w_req}x{h_req}' did not attach (server refused it)"
            )));
        }
        Ok(())
    }

    /// Keep every connected output at the refresh `target_refresh` makes of `refresh`,
    /// each keeping its geometry and position and taking a mode of the same size at the
    /// new refresh, so the screen, the monitors, and every capture stay as they are.
    ///
    /// Returns the refresh of each mode set; empty where every output's mode already runs
    /// at the target or carries no timings to change.
    pub fn refresh_output_mode(&mut self, refresh: Option<f64>) -> Result<Vec<f64>> {
        let mut state = self.connected_output_state()?;
        let target = target_refresh(refresh);
        let ts = state.res.config_timestamp;
        let mut rates = Vec::new();
        for &out_id in &state.res.outputs.clone() {
            let oi = self.output_info(out_id, ts)?;
            if oi.connection != randr::Connection::CONNECTED || oi.crtc == 0 {
                continue;
            }
            let ci = self.crtc_info(oi.crtc, ts)?;
            let Some(mode) = state.pool.mode(ci.mode).copied() else {
                continue;
            };
            let rate = mode.refresh();
            if rate == 0.0 || (rate - target).abs() <= target * modes::REFRESH_SLACK {
                continue;
            }
            let prefix = if state.pool.name_of(mode.id).starts_with("selkies-") {
                "selkies-"
            } else {
                ""
            };
            state.pool.on_output = oi.modes.clone();
            let (mode_id, rate) = mode_at(
                self,
                &mut state.pool,
                out_id,
                mode.width,
                mode.height,
                target,
                &format!("{prefix}{}x{}", mode.width, mode.height),
            )?;
            let status = self
                .conn
                .randr_set_crtc_config(
                    oi.crtc,
                    x11rb::CURRENT_TIME,
                    ts,
                    ci.x,
                    ci.y,
                    mode_id,
                    ci.rotation,
                    &ci.outputs,
                )?
                .reply()?
                .status;
            if status != SetConfig::SUCCESS {
                return Err(DisplayError::Refused(format!(
                    "SetCrtcConfig returned status {status:?}"
                )));
            }
            rates.push(rate);
        }
        self.sync()?;
        Ok(rates)
    }

    /// The RandR mode-create/activate/screen-size sequence.
    ///
    /// The screen takes the width rounded up to the 8-pixel CVT cell, which is what the
    /// modes created for it carry; they are named for that geometry because a name that
    /// disagrees with the pixel size breaks later xrandr calls that derive framebuffer
    /// dimensions from it. The mode is chosen or made at the refresh `target_refresh` makes
    /// of `refresh`, the stream's frame rate (`mode_at`). `output_size`, the one display of
    /// a logical-monitor layout, has the output show exactly that rectangle from the origin
    /// instead of the whole screen: Qt takes a monitor's geometry from its CRTC and
    /// announces a change only where the two agree, so a monitor narrower than its CRTC
    /// leaves a Qt desktop at the size it had. The physical size follows the DPI the last
    /// `set_dpi` stamped (96 when never retargeted): xdpyinfo and the toolkit paths reading
    /// RandR's physical size would otherwise un-scale after every resize. The screen may not
    /// shrink under an active CRTC, so a CRTC that would poke out of the new screen is
    /// disabled first, as xrandr does.
    ///
    /// Returns the screen's size and the refresh of the mode set.
    pub fn resize_on_display(
        &mut self,
        w_req: i32,
        h_req: i32,
        refresh: Option<f64>,
        output_size: Option<(i32, i32)>,
    ) -> Result<(i32, i32, f64)> {
        if w_req <= 0 || h_req <= 0 {
            return Err(DisplayError::Invalid(format!(
                "invalid resolution '{w_req}x{h_req}'"
            )));
        }
        let mut state = self.connected_output_state()?;
        let (screen_w, screen_h) = ((w_req as u32).div_ceil(8) * 8, h_req as u32);
        let target = target_refresh(refresh);
        let (mut out_w, mut out_h) = output_size
            .map(|(w, h)| (w.max(0) as u32, h.max(0) as u32))
            .unwrap_or((screen_w, screen_h));
        if out_w > screen_w || out_h > screen_h {
            (out_w, out_h) = (screen_w, screen_h);
        }
        let (mode_id, rate) = if (out_w, out_h) == (screen_w, screen_h) {
            mode_at(
                self,
                &mut state.pool,
                state.out_id,
                screen_w,
                screen_h,
                target,
                &format!("{screen_w}x{screen_h}"),
            )?
        } else {
            mode_at(
                self,
                &mut state.pool,
                state.out_id,
                out_w,
                out_h,
                target,
                &format!("selkies-{out_w}x{out_h}"),
            )?
        };
        let crtc = if state.oi.crtc != 0 {
            state.oi.crtc
        } else {
            state.oi.crtcs.first().copied().unwrap_or(0)
        };
        if crtc == 0 {
            return Err(DisplayError::Refused("output has no usable CRTC".into()));
        }
        let ts = state.res.config_timestamp;
        let ci = self.crtc_info(crtc, ts)?;
        let outputs = if ci.outputs.is_empty() {
            vec![state.out_id]
        } else {
            ci.outputs.clone()
        };
        let (geom_w, geom_h) = self.root_size()?;
        let (mm_w, mm_h) = (millimeters(screen_w as i32), millimeters(screen_h as i32));
        let rotation = if u16::from(ci.rotation) == 0 {
            randr::Rotation::ROTATE0
        } else {
            ci.rotation
        };
        let (crtc_x, crtc_y) = if output_size.is_none() {
            (ci.x, ci.y)
        } else {
            (0, 0)
        };
        let crtc_fits = ci.x as i32 + ci.width as i32 <= screen_w as i32
            && ci.y as i32 + ci.height as i32 <= screen_h as i32;
        let root = self.root;
        self.grabbed(|x| {
            if ci.mode != 0 && !crtc_fits {
                let status = x
                    .conn
                    .randr_set_crtc_config(
                        crtc,
                        x11rb::CURRENT_TIME,
                        ts,
                        ci.x,
                        ci.y,
                        0,
                        rotation,
                        &[],
                    )?
                    .reply()?
                    .status;
                if status != SetConfig::SUCCESS {
                    return Err(DisplayError::Refused(format!(
                        "CRTC disable returned status {status:?}"
                    )));
                }
            }
            if (geom_w as u32, geom_h as u32) != (screen_w, screen_h) {
                x.conn
                    .randr_set_screen_size(root, screen_w as u16, screen_h as u16, mm_w, mm_h)?
                    .check()?;
            }
            let status = x
                .conn
                .randr_set_crtc_config(
                    crtc,
                    x11rb::CURRENT_TIME,
                    ts,
                    crtc_x,
                    crtc_y,
                    mode_id,
                    rotation,
                    &outputs,
                )?
                .reply()?
                .status;
            if status != SetConfig::SUCCESS {
                return Err(DisplayError::Refused(format!(
                    "SetCrtcConfig returned status {status:?}"
                )));
            }
            Ok(())
        })?;
        self.sync()?;
        let (got_w, got_h) = self.root_size()?;
        if (got_w as u32, got_h as u32) != (screen_w, screen_h) {
            return Err(DisplayError::Refused(format!(
                "screen is {got_w}x{got_h} after applying '{w_req}x{h_req}'"
            )));
        }
        Ok((screen_w as i32, screen_h as i32, rate))
    }

    /// `RRSetScreenSize` to exactly `w` by `h`, with no CRTC change: what sizes the root of
    /// a server with no connected output, where there is no mode to set and the server
    /// sizes the framebuffer freely in both directions. Returns the root's size afterwards.
    pub fn set_screen_size(&mut self, w: i32, h: i32) -> Result<(i32, i32)> {
        self.conn
            .randr_set_screen_size(
                self.root,
                w as u16,
                h as u16,
                millimeters(w),
                millimeters(h),
            )?
            .check()?;
        self.sync()?;
        self.root_size()
    }

    /// Grow-only screen resize (never shrinks; no CRTC change).
    pub fn grow_screen(&mut self, w: i32, h: i32) -> Result<()> {
        let (cur_w, cur_h) = self.root_size()?;
        if cur_w >= w && cur_h >= h {
            return Ok(());
        }
        self.conn
            .randr_set_screen_size(
                self.root,
                w as u16,
                h as u16,
                millimeters(w),
                millimeters(h),
            )?
            .check()?;
        self.sync()?;
        let (got_w, got_h) = self.root_size()?;
        if got_w < w || got_h < h {
            // A short root is the server's answer, not a broken connection: `Refused`
            // keeps the connection cached.
            return Err(DisplayError::Refused(format!(
                "screen is {got_w}x{got_h} after grow to {w}x{h}"
            )));
        }
        Ok(())
    }

    /// Resize the root pane's reported physical size to match `dpi`, keeping the DPI the X
    /// server itself reports (xdpyinfo/RandR consumers) in step with the DPI the desktops
    /// were told to render at. Idempotent: a matching mm size posts no `RRSetScreenSize` at
    /// all, so the idleness of repeated SETTINGS payloads stays ConfigureNotify-free.
    pub fn stamp_root_dpi(&mut self, dpi: i32) -> Result<()> {
        let (w, h) = self.root_size()?;
        let mm_w = ((w as f64 * 25.4 / dpi as f64).round() as u32).max(1);
        let mm_h = ((h as f64 * 25.4 / dpi as f64).round() as u32).max(1);
        let info = self.conn.randr_get_screen_info(self.root)?.reply()?;
        let current = info.sizes.get(info.size_id as usize);
        if current.is_some_and(|s| (s.mwidth as u32, s.mheight as u32) == (mm_w, mm_h)) {
            set_applied_dpi(dpi);
            return Ok(());
        }
        self.conn
            .randr_set_screen_size(self.root, w as u16, h as u16, mm_w, mm_h)?
            .check()?;
        self.sync()?;
        set_applied_dpi(dpi);
        Ok(())
    }

    /// The `RESOURCE_MANAGER` property of the root: the server's resource database.
    pub fn resource_manager_text(&self) -> Result<Option<String>> {
        let reply = self
            .conn
            .get_property(
                false,
                self.root,
                AtomEnum::RESOURCE_MANAGER,
                AtomEnum::STRING,
                0,
                u32::MAX,
            )?
            .reply()?;
        if reply.value.is_empty() {
            return Ok(None);
        }
        Ok(Some(reply.value.iter().map(|&b| b as char).collect()))
    }

    /// The outputs other than `primary_out` that carry the `Connected` property.
    fn pluggable_outputs(
        &mut self,
        res: &randr::GetScreenResourcesReply,
        primary_out: u32,
    ) -> Result<Vec<u32>> {
        let connected = self.atom(CONNECTED_PROP)?;
        let mut spare = Vec::new();
        for &out_id in &res.outputs {
            if out_id == primary_out {
                continue;
            }
            if self
                .conn
                .randr_list_output_properties(out_id)?
                .reply()?
                .atoms
                .contains(&connected)
            {
                spare.push(out_id);
            }
        }
        Ok(spare)
    }

    /// Whether this X server lets a display be added as an output of its own; false on a
    /// server without a connected output.
    pub fn has_pluggable_outputs(&mut self) -> Result<bool> {
        match self.connected_output_state() {
            Ok(state) => Ok(!self.pluggable_outputs(&state.res, state.out_id)?.is_empty()),
            Err(DisplayError::NoOutput) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Resolve or create a mode of exactly `w` by `h` at `refresh` on `out_id`.
    ///
    /// A display's output is the rectangle its client streams, so the width is not rounded
    /// up to the CVT cell as `resize_on_display` does for a mode covering the whole
    /// framebuffer: two outputs side by side would overlap by the difference. The name says
    /// which kind it is, because a `WxH` name is looked up by both.
    fn exact_mode(
        &mut self,
        pool: &mut ModePool,
        out_id: u32,
        ts: u32,
        w: u32,
        h: u32,
        refresh: f64,
    ) -> Result<u32> {
        let oi = self.output_info(out_id, ts)?;
        pool.on_output = oi.modes;
        Ok(mode_at(
            self,
            pool,
            out_id,
            w,
            h,
            refresh,
            &format!("selkies-{w}x{h}"),
        )?
        .0)
    }

    /// Configure a CRTC, unless it is configured so already.
    fn set_crtc(
        &mut self,
        crtc: u32,
        ts: u32,
        x: i32,
        y: i32,
        mode_id: u32,
        outputs: &[u32],
    ) -> Result<()> {
        let ci = self.crtc_info(crtc, ts)?;
        if (ci.x as i32, ci.y as i32, ci.mode, ci.outputs.as_slice()) == (x, y, mode_id, outputs) {
            return Ok(());
        }
        let status = self
            .conn
            .randr_set_crtc_config(
                crtc,
                x11rb::CURRENT_TIME,
                ts,
                x as i16,
                y as i16,
                mode_id,
                randr::Rotation::ROTATE0,
                outputs,
            )?
            .reply()?
            .status;
        if status != SetConfig::SUCCESS {
            return Err(DisplayError::Refused(format!(
                "SetCrtcConfig returned status {status:?}"
            )));
        }
        Ok(())
    }

    /// Plug output `out_id` in or out through its `Connected` property.
    fn plug(&mut self, out_id: u32, plugged: bool) -> Result<()> {
        let prop = self.atom(CONNECTED_PROP)?;
        let value = u32::from(plugged).to_ne_bytes();
        self.conn
            .randr_change_output_property(
                out_id,
                prop,
                AtomEnum::INTEGER.into(),
                32,
                PropMode::REPLACE,
                1,
                &value,
            )?
            .check()?;
        Ok(())
    }

    /// The window manager's clients with their rectangles in root coordinates, less the
    /// self-placing kinds; empty where no manager publishes `_NET_CLIENT_LIST`.
    pub fn client_windows(&mut self) -> Result<Vec<WindowRect>> {
        let list_atom = self.atom("_NET_CLIENT_LIST")?;
        let listed = self.prop32(self.root, list_atom, AtomEnum::ANY)?;
        let kind_atom = self.atom("_NET_WM_WINDOW_TYPE")?;
        let mut self_placing = Vec::new();
        for name in SELF_PLACING_WINDOW_TYPES {
            self_placing.push(self.atom(name)?);
        }
        let mut windows = Vec::new();
        for wid in listed {
            let Ok(kind) = self.prop32(wid, kind_atom, AtomEnum::ANY) else {
                continue;
            };
            if kind.iter().any(|a| self_placing.contains(a)) {
                continue;
            }
            let Ok((x, y, w, h)) = self.window_rect(wid) else {
                continue;
            };
            windows.push((wid, x, y, w, h));
        }
        Ok(windows)
    }

    /// Each display's rectangle from its output's CRTC, for the outputs driving one.
    fn output_rects(
        &self,
        ts: u32,
        outputs: &BTreeMap<String, u32>,
    ) -> Result<BTreeMap<String, Rect>> {
        let mut rects = BTreeMap::new();
        for (did, &out_id) in outputs {
            let oi = self.output_info(out_id, ts)?;
            if oi.crtc != 0 {
                let ci = self.crtc_info(oi.crtc, ts)?;
                if ci.mode != 0 {
                    rects.insert(
                        did.clone(),
                        (ci.x as i32, ci.y as i32, ci.width as i32, ci.height as i32),
                    );
                }
            }
        }
        Ok(rects)
    }

    /// Ask the manager to maximize `win` both ways, or to restore it.
    fn set_maximized(&mut self, win: Window, on: bool) -> Result<()> {
        let horz = self.atom("_NET_WM_STATE_MAXIMIZED_HORZ")?;
        let vert = self.atom("_NET_WM_STATE_MAXIMIZED_VERT")?;
        self.send_to_manager(win, "_NET_WM_STATE", [u32::from(on), horz, vert, 2, 0])
    }

    fn is_maximized(&mut self, win: Window) -> Result<bool> {
        let state_atom = self.atom("_NET_WM_STATE")?;
        let horz = self.atom("_NET_WM_STATE_MAXIMIZED_HORZ")?;
        let vert = self.atom("_NET_WM_STATE_MAXIMIZED_VERT")?;
        let state = self.prop32(win, state_atom, AtomEnum::ANY)?;
        Ok(state.iter().any(|&a| a == horz || a == vert))
    }

    /// Ask the window manager for every move `window_moves` wants, each an
    /// `_NET_MOVERESIZE_WINDOW` with static gravity, so the request names the client
    /// window's own corner and the frame around it stays the manager's business, from a
    /// pager's source indication so it is honored as a user's own move would be.
    ///
    /// The result is read back and a window not where it was asked to go is asked again,
    /// for a bounded moment: a manager still taking in the new screen re-places the window
    /// by its own rule after the request (KWin does, for a display added above), and the
    /// later ask is the one that stands. A maximized window is measured by the screen its
    /// center is on, since its corner is the manager's; one the manager kept on the screen
    /// it was maximized on (xfwm4 maximizes a window again on the monitor its restored
    /// geometry falls on) is restored for the move and maximized again once it has landed.
    ///
    /// Returns how many windows were asked to move.
    pub fn follow_display_moves(
        &mut self,
        windows: &[WindowRect],
        before: &BTreeMap<String, Rect>,
        after: &BTreeMap<String, Rect>,
    ) -> Result<usize> {
        let moves = window_moves(windows, before, after);
        if moves.is_empty() {
            return Ok(0);
        }
        let flags = move_flags();
        let sizes: HashMap<Window, (i32, i32)> = windows
            .iter()
            .map(|&(w, _, _, ww, hh)| (w, (ww, hh)))
            .collect();
        let screen_of = |x: i32, y: i32, w: i32, h: i32| -> Option<Rect> {
            let (cx, cy) = (x + w / 2, y + h / 2);
            after
                .values()
                .find(|r| r.0 <= cx && cx < r.0 + r.2 && r.1 <= cy && cy < r.1 + r.3)
                .copied()
        };
        let mut pending: BTreeMap<Window, (i32, i32)> =
            moves.iter().map(|&(w, x, y)| (w, (x, y))).collect();
        let mut restored: HashSet<Window> = HashSet::new();
        let deadline = Instant::now() + MANAGER_SETTLE;
        let mut rounds = 0;
        while !pending.is_empty() {
            for (&win, &(x, y)) in &pending.clone() {
                self.send_to_manager(
                    win,
                    "_NET_MOVERESIZE_WINDOW",
                    [flags, x as u32, y as u32, 0, 0],
                )?;
            }
            self.flush()?;
            std::thread::sleep(MANAGER_POLL);
            rounds += 1;
            for (win, (x, y)) in pending.clone() {
                let (Ok((at_x, at_y, _, _)), Ok(maximized)) =
                    (self.window_rect(win), self.is_maximized(win))
                else {
                    pending.remove(&win);
                    continue;
                };
                let (w, h) = sizes.get(&win).copied().unwrap_or((0, 0));
                let landed = ((at_x - x).abs() <= MOVE_SLACK && (at_y - y).abs() <= MOVE_SLACK)
                    || (maximized && screen_of(at_x, at_y, w, h) == screen_of(x, y, w, h));
                if landed {
                    pending.remove(&win);
                    if restored.contains(&win) {
                        self.set_maximized(win, true)?;
                    }
                } else if maximized && rounds > 1 && !restored.contains(&win) {
                    restored.insert(win);
                    self.set_maximized(win, false)?;
                }
            }
            if Instant::now() >= deadline {
                break;
            }
        }
        for &win in &restored {
            if pending.contains_key(&win) {
                self.set_maximized(win, true)?;
            }
        }
        self.flush()?;
        debug!(
            target: "display",
            "Asked the window manager to move {} window(s) with their displays; {} not there after {rounds} round(s).",
            moves.len(), pending.len()
        );
        Ok(moves.len())
    }

    /// Blocking layout of every display as an output of its own.
    ///
    /// The primary is the output the server started with; each other display keeps the
    /// pluggable output it holds or takes a free one, since to the desktop a display that
    /// stays is one monitor, not one unplugged and another added. Under one server grab, so
    /// a window manager acts on the finished arrangement only: outputs no display uses are
    /// switched off and unplugged, the screen grows to hold the old and the new arrangement
    /// at once (a CRTC may never poke out of the screen), each display's output is plugged
    /// in and its CRTC given the display's exact mode, at the refresh `target_refresh` makes
    /// of `refresh` (the stream's frame rate), at the display's position, and the screen
    /// shrinks to the total. Logical monitors a previous layout defined are deleted, since
    /// the server derives a monitor from every active CRTC once none is defined.
    ///
    /// Fails if there are more displays than outputs, or the server refused a step; the
    /// caller falls back to the logical monitors.
    pub fn apply_output_layout(
        &mut self,
        layouts: &Layouts,
        total_w: i32,
        total_h: i32,
        refresh: Option<f64>,
    ) -> Result<()> {
        let mut state = self.connected_output_state()?;
        let primary_out = state.out_id;
        let spare = self.pluggable_outputs(&state.res, primary_out)?;
        if layouts.len() > 1 + spare.len() {
            return Err(DisplayError::Refused(format!(
                "{} displays but {} outputs",
                layouts.len(),
                1 + spare.len()
            )));
        }
        let ts = state.res.config_timestamp;
        let windows = self.client_windows()?;
        let mut tracked: BTreeMap<String, u32> = output_of().clone();
        tracked.insert(PRIMARY_DISPLAY.into(), primary_out);
        let before = self.output_rects(ts, &tracked)?;
        let held: BTreeMap<String, u32> = output_of()
            .iter()
            .filter(|(did, out)| layouts.contains_key(*did) && spare.contains(out))
            .map(|(d, o)| (d.clone(), *o))
            .collect();
        let mut free: Vec<u32> = spare
            .iter()
            .copied()
            .filter(|o| !held.values().any(|h| h == o))
            .collect();
        let mut assigned: Vec<(u32, String, super::Layout)> = Vec::new();
        for (did, l) in layouts {
            let out = if did == PRIMARY_DISPLAY {
                primary_out
            } else if let Some(&o) = held.get(did) {
                o
            } else {
                free.remove(0)
            };
            assigned.push((out, did.clone(), *l));
        }
        {
            let mut map = output_of();
            map.clear();
            for (out, did, _) in &assigned {
                if did != PRIMARY_DISPLAY {
                    map.insert(did.clone(), *out);
                }
            }
        }
        let target = target_refresh(refresh);
        let root = self.root;
        self.grabbed(|x| {
            super::monitors::drop_selkies_monitors(x)?;
            let (geom_w, geom_h) = x.root_size()?;
            x.size_screen_if_changed(geom_w.max(total_w), geom_h.max(total_h))?;
            for out_id in std::iter::once(primary_out).chain(spare.iter().copied()) {
                if assigned.iter().any(|(o, _, _)| *o == out_id) {
                    continue;
                }
                let oi = x.output_info(out_id, ts)?;
                if oi.crtc != 0 {
                    x.set_crtc(oi.crtc, ts, 0, 0, 0, &[])?;
                }
                if oi.connection == randr::Connection::CONNECTED {
                    x.plug(out_id, false)?;
                }
            }
            for (out_id, _, l) in &assigned {
                let oi = x.output_info(*out_id, ts)?;
                if *out_id != primary_out && oi.connection != randr::Connection::CONNECTED {
                    x.plug(*out_id, true)?;
                }
                let crtc = if oi.crtc != 0 {
                    oi.crtc
                } else {
                    oi.crtcs.first().copied().unwrap_or(0)
                };
                if crtc == 0 {
                    return Err(DisplayError::Refused(format!(
                        "output {} has no usable CRTC",
                        String::from_utf8_lossy(&oi.name)
                    )));
                }
                let mode_id =
                    x.exact_mode(&mut state.pool, *out_id, ts, l.w as u32, l.h as u32, target)?;
                x.set_crtc(crtc, ts, l.x, l.y, mode_id, &[*out_id])?;
            }
            x.size_screen_if_changed(total_w, total_h)?;
            x.conn
                .randr_set_output_primary(root, primary_out)?
                .check()?;
            Ok(())
        })?;
        self.sync()?;
        for (out_id, did, l) in &assigned {
            let oi = self.output_info(*out_id, ts)?;
            let got = if oi.crtc != 0 {
                let ci = self.crtc_info(oi.crtc, ts)?;
                Some((ci.x as i32, ci.y as i32, ci.width as i32, ci.height as i32))
            } else {
                None
            };
            if got != Some(l.rect()) {
                return Err(DisplayError::Refused(format!(
                    "display '{did}' realized as {got:?}"
                )));
            }
        }
        let after: BTreeMap<String, Rect> =
            layouts.iter().map(|(d, l)| (d.clone(), l.rect())).collect();
        self.follow_display_moves(&windows, &before, &after)?;
        seat_desktop_windows();
        Ok(())
    }

    /// Set the screen size only when it changes: Xvfb rebuilds its framebuffer on every
    /// screen size set, the same size included, and the desktop is blank until every
    /// window repaints.
    fn size_screen_if_changed(&mut self, w: i32, h: i32) -> Result<()> {
        if self.root_size()? != (w, h) {
            self.conn
                .randr_set_screen_size(
                    self.root,
                    w as u16,
                    h as u16,
                    millimeters(w),
                    millimeters(h),
                )?
                .check()?;
        }
        Ok(())
    }

    /// `output_layout_stage` against the outputs as they are now.
    pub fn output_stage(&mut self, layouts: &Layouts) -> Result<Option<Layouts>> {
        let state = self.connected_output_state()?;
        let ts = state.res.config_timestamp;
        let mut plugged = HashSet::new();
        for out_id in self.pluggable_outputs(&state.res, state.out_id)? {
            if self.output_info(out_id, ts)?.connection == randr::Connection::CONNECTED {
                plugged.insert(out_id);
            }
        }
        let live: Vec<String> = output_of()
            .iter()
            .filter(|(_, o)| plugged.contains(o))
            .map(|(d, _)| d.clone())
            .collect();
        let primary_at = if state.oi.crtc != 0 {
            let ci = self.crtc_info(state.oi.crtc, ts)?;
            (ci.mode != 0).then_some((ci.x as i32, ci.y as i32))
        } else {
            None
        };
        Ok(super::geometry::output_layout_stage(
            primary_at, &live, layouts,
        ))
    }

    /// Return to the one output the server started with: every pluggable output switched
    /// off and unplugged, the primary's CRTC back at the origin. The screen keeps its size,
    /// which the resize that follows a teardown sets. A no-op on a server without pluggable
    /// outputs.
    pub fn retire_outputs(&mut self) -> Result<()> {
        let state = match self.connected_output_state() {
            Ok(state) => state,
            Err(DisplayError::NoOutput) => return Ok(()),
            Err(e) => return Err(e),
        };
        let primary_out = state.out_id;
        let spare = self.pluggable_outputs(&state.res, primary_out)?;
        if spare.is_empty() {
            return Ok(());
        }
        let ts = state.res.config_timestamp;
        let windows = self.client_windows()?;
        let mut tracked: BTreeMap<String, u32> = output_of().clone();
        tracked.insert(PRIMARY_DISPLAY.into(), primary_out);
        let before = self.output_rects(ts, &tracked)?;
        output_of().clear();
        let poi = state.oi;
        self.grabbed(|x| {
            for &out_id in &spare {
                let oi = x.output_info(out_id, ts)?;
                if oi.crtc != 0 {
                    x.set_crtc(oi.crtc, ts, 0, 0, 0, &[])?;
                }
                if oi.connection == randr::Connection::CONNECTED {
                    x.plug(out_id, false)?;
                }
            }
            if poi.crtc != 0 {
                let ci = x.crtc_info(poi.crtc, ts)?;
                if ci.mode != 0 && (ci.x, ci.y) != (0, 0) {
                    x.set_crtc(poi.crtc, ts, 0, 0, ci.mode, &ci.outputs)?;
                }
            }
            Ok(())
        })?;
        self.sync()?;
        if let Some(&(_, _, w, h)) = before.get(PRIMARY_DISPLAY) {
            let after: BTreeMap<String, Rect> = [(PRIMARY_DISPLAY.to_string(), (0, 0, w, h))]
                .into_iter()
                .collect();
            self.follow_display_moves(&windows, &before, &after)?;
        }
        seat_desktop_windows();
        Ok(())
    }

    /// The running EWMH window manager's check window (root `_NET_SUPPORTING_WM_CHECK`),
    /// `None` when no manager advertises one.
    fn wm_check_window(&mut self) -> Result<Option<Window>> {
        let check = self.atom("_NET_SUPPORTING_WM_CHECK")?;
        Ok(self
            .prop32(self.root, check, AtomEnum::WINDOW)?
            .first()
            .copied())
    }

    /// Name of the running EWMH window manager, empty when none: the check window's
    /// `_NET_WM_NAME`.
    pub fn wm_name(&mut self) -> Result<String> {
        let Some(win) = self.wm_check_window()? else {
            return Ok(String::new());
        };
        let name = self.atom("_NET_WM_NAME")?;
        let utf8 = self.atom("UTF8_STRING")?;
        let reply = self
            .conn
            .get_property(false, win, name, utf8, 0, u32::MAX)?
            .reply()?;
        Ok(String::from_utf8_lossy(&reply.value).into_owned())
    }

    /// Process id of the running EWMH window manager, 0 when none: the check window's
    /// `_NET_WM_PID`, or, for a manager that sets none (Openbox), what the X-Resource
    /// extension knows of the client owning that window.
    pub fn wm_pid(&mut self) -> Result<i32> {
        let Some(win) = self.wm_check_window()? else {
            return Ok(0);
        };
        let pid_atom = self.atom("_NET_WM_PID")?;
        if let Some(&pid) = self.prop32(win, pid_atom, AtomEnum::CARDINAL)?.first() {
            return Ok(pid as i32);
        }
        if self
            .conn
            .extension_information(res::X11_EXTENSION_NAME)?
            .is_none()
        {
            return Ok(0);
        }
        let spec = res::ClientIdSpec {
            client: win,
            mask: res::ClientIdMask::LOCAL_CLIENT_PID,
        };
        let reply = self.conn.res_query_client_ids(&[spec])?.reply()?;
        for entry in reply.ids {
            if u32::from(entry.spec.mask) & u32::from(res::ClientIdMask::LOCAL_CLIENT_PID) != 0
                && let Some(&pid) = entry.value.first()
            {
                return Ok(pid as i32);
            }
        }
        Ok(0)
    }

    /// The RandR 1.5 monitors defined on the root, active or not.
    pub fn monitors(&self) -> Result<Vec<randr::MonitorInfo>> {
        Ok(self
            .conn
            .randr_get_monitors(self.root, false)?
            .reply()?
            .monitors)
    }

    /// A monitor's name.
    pub fn monitor_name(&self, m: &randr::MonitorInfo) -> Result<String> {
        self.atom_name(m.name)
    }

    /// Define logical monitor `info`.
    pub fn set_monitor(&self, info: randr::MonitorInfo) -> Result<()> {
        self.conn.randr_set_monitor(self.root, info)?.check()?;
        Ok(())
    }

    /// Delete the logical monitor named `name`.
    pub fn delete_monitor(&mut self, name: &str) -> Result<()> {
        let atom = self.atom(name)?;
        self.conn.randr_delete_monitor(self.root, atom)?.check()?;
        Ok(())
    }

    /// Flag `output` as the primary; 0 clears the flag.
    pub fn set_output_primary(&self, output: u32) -> Result<()> {
        self.conn
            .randr_set_output_primary(self.root, output)?
            .check()?;
        Ok(())
    }

    /// Set a 32-bit integer output property.
    pub fn set_output_property_u32(&mut self, output: u32, name: &str, value: u32) -> Result<()> {
        let prop = self.atom(name)?;
        self.conn
            .randr_change_output_property(
                output,
                prop,
                AtomEnum::INTEGER.into(),
                32,
                PropMode::REPLACE,
                1,
                &value.to_ne_bytes(),
            )?
            .check()?;
        Ok(())
    }
}

impl ModeServer for X11 {
    fn create_mode(&mut self, timings: &ModeTimings, name: &str) -> Result<u32> {
        let info = ModeInfo {
            id: 0,
            width: timings.width as u16,
            height: timings.height as u16,
            dot_clock: timings.dot_clock as u32,
            hsync_start: timings.h_sync_start as u16,
            hsync_end: timings.h_sync_end as u16,
            htotal: timings.h_total as u16,
            hskew: timings.h_skew as u16,
            vsync_start: timings.v_sync_start as u16,
            vsync_end: timings.v_sync_end as u16,
            vtotal: timings.v_total as u16,
            name_len: name.len() as u16,
            mode_flags: randr::ModeFlag::from(timings.flags),
        };
        Ok(self
            .conn
            .randr_create_mode(self.root, info, name.as_bytes())?
            .reply()?
            .mode)
    }

    fn add_output_mode(&mut self, output: u32, mode: u32) -> Result<()> {
        self.conn.randr_add_output_mode(output, mode)?.check()?;
        Ok(())
    }
}

/// The module's retained connection, behind the one lock every X helper takes.
static SESSION: Mutex<Option<X11>> = Mutex::new(None);

/// Run `f` on the retained connection, connecting first where there is none, and drop the
/// connection on any failure other than an X protocol error. Blocking; call through
/// `on_x11` from async code.
pub fn with_x11<T>(f: impl FnOnce(&mut X11) -> Result<T>) -> Result<T> {
    let mut slot = SESSION.lock().unwrap_or_else(|e| e.into_inner());
    if slot.is_none() {
        let mut x = X11::connect(BLOCKING_TIMEOUT)?;
        x.sync()?;
        x.note_configured_refresh();
        x.reap_retained_predecessors();
        *slot = Some(x);
    }
    let x = slot.as_mut().expect("connection present");
    let result = f(x);
    if let Err(e) = &result
        && !e.is_protocol()
    {
        *slot = None;
    }
    result
}

/// Close and forget the retained connection so the next call reconnects.
pub fn drop_connection() {
    *SESSION.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// `with_x11` on a blocking thread of the runtime.
pub async fn on_x11<T: Send + 'static>(
    f: impl FnOnce(&mut X11) -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(move || with_x11(f)).await?
}

/// Keep a desktop's background window at the screen's origin while the desktop takes in a
/// new arrangement.
///
/// pcmanfm-qt draws one window over the union of the screens on X11 and, told of the
/// screens one at a time, first moves it to the primary's new place and then grows it to
/// the union; Openbox keeps the top or left edge of a window growing that way on the monitor
/// most of it is on, so a display added above or left of the primary leaves the grown window
/// at the primary's offset with the union's size, blank on the new display and showing it
/// the primary's share of the wallpaper. A desktop window reaching past the screen is asked,
/// as a pager's move of its own corner, to sit at the origin, which a manager grants since
/// nothing grows. The desktop's asks follow the manager's notices by a moment of their own,
/// so the screen is watched for `settle`, on a connection of this call's own since it runs
/// beside the layout's thread.
///
/// Returns how many windows were seated.
pub fn seat_desktop_windows_blocking(settle: Duration) -> usize {
    let mut x = match X11::connect(BLOCKING_TIMEOUT) {
        Ok(x) => x,
        Err(e) => {
            debug!(target: "display", "No X connection to watch the desktop window on ({e}).");
            return 0;
        }
    };
    let mut seated: HashSet<Window> = HashSet::new();
    let watch = |x: &mut X11, seated: &mut HashSet<Window>| -> Result<()> {
        let list_atom = x.atom("_NET_CLIENT_LIST")?;
        let kind_atom = x.atom("_NET_WM_WINDOW_TYPE")?;
        let desktop_atom = x.atom("_NET_WM_WINDOW_TYPE_DESKTOP")?;
        let flags = move_flags();
        let deadline = Instant::now() + settle;
        loop {
            let (screen_w, screen_h) = x.root_size()?;
            for wid in x.prop32(x.root, list_atom, AtomEnum::ANY)? {
                let Ok(kind) = x.prop32(wid, kind_atom, AtomEnum::ANY) else {
                    continue;
                };
                if !kind.contains(&desktop_atom) {
                    continue;
                }
                let Ok((at_x, at_y, w, h)) = x.window_rect(wid) else {
                    continue;
                };
                if (at_x, at_y) != (0, 0) && (at_x + w > screen_w || at_y + h > screen_h) {
                    x.send_to_manager(wid, "_NET_MOVERESIZE_WINDOW", [flags, 0, 0, 0, 0])?;
                    x.flush()?;
                    seated.insert(wid);
                }
            }
            if Instant::now() >= deadline {
                return Ok(());
            }
            std::thread::sleep(MANAGER_POLL);
        }
    };
    if let Err(e) = watch(&mut x, &mut seated) {
        debug!(target: "display", "Stopped watching the desktop window ({e}).");
    }
    if !seated.is_empty() {
        debug!(target: "display", "Asked the window manager to seat {} desktop window(s) at the origin.", seated.len());
    }
    seated.len()
}

/// Watch for a desktop window the arrangement just applied left away from the origin,
/// beside the layout's own thread so no display waits on the desktop's own reaction.
pub fn seat_desktop_windows() {
    let spawned = std::thread::Builder::new()
        .name("selkies-desktop-seat".into())
        .spawn(|| {
            seat_desktop_windows_blocking(DESKTOP_SETTLE);
        });
    if let Err(e) = spawned {
        debug!(target: "display", "Desktop window watch not started: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Resize the harness display and read the realized geometry back, then restore it.
    #[test]
    #[ignore = "needs an X server with RandR on DISPLAY"]
    fn live_resize_round_trip() {
        let before = with_x11(|x| x.query_randr()).expect("query");
        let (w, h) = before.current;
        let realized =
            with_x11(|x| x.resize_on_display(1280, 720, Some(60.0), None)).expect("resize");
        assert_eq!((realized.0, realized.1), (1280, 720));
        assert!(realized.2 > 59.0 && realized.2 < 61.0, "{}", realized.2);
        let after = with_x11(|x| x.query_randr()).expect("query");
        assert_eq!(after.current, (1280, 720));
        assert_eq!(after.output_name, before.output_name);
        assert!(
            before
                .resolutions
                .iter()
                .all(|r| after.resolutions.contains(r)),
            "a resize takes no mode off the output: {:?} vs {:?}",
            before.resolutions,
            after.resolutions
        );
        with_x11(|x| x.resize_on_display(w, h, None, None)).expect("restore");
        assert_eq!(
            with_x11(|x| x.query_randr()).expect("query").current,
            before.current
        );
    }

    /// Pluggable outputs are detected, laid out, and retired on the harness display.
    #[test]
    #[ignore = "needs the XLibre Xvfb with pluggable outputs on DISPLAY"]
    fn live_output_layout() {
        let before = with_x11(|x| x.query_randr()).expect("query").current;
        let pluggable = with_x11(|x| x.has_pluggable_outputs()).expect("probe");
        if !pluggable {
            eprintln!("no pluggable outputs on this server; skipping the layout");
            return;
        }
        let layouts: Layouts = [
            (
                PRIMARY_DISPLAY.to_string(),
                super::super::Layout::new(0, 0, 1280, 720),
            ),
            (
                "display2".to_string(),
                super::super::Layout::new(1280, 0, 1024, 768),
            ),
        ]
        .into_iter()
        .collect();
        let staged = with_x11(|x| x.output_stage(&layouts)).expect("stage");
        assert_eq!(
            staged, None,
            "a display added beside a primary that stays needs no stage"
        );
        with_x11(|x| x.apply_output_layout(&layouts, 2304, 768, Some(60.0))).expect("layout");
        assert_eq!(
            with_x11(|x| x.query_randr()).expect("query").current,
            (2304, 768)
        );
        with_x11(|x| x.retire_outputs()).expect("retire");
        with_x11(|x| x.resize_on_display(before.0, before.1, None, None)).expect("restore");
    }
}
