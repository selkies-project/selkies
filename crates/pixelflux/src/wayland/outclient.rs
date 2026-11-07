//! `zwlr_output_management_v1` client: scales and arranges the screens of a
//! nested session compositor, in-process rather than through a `wlr-randr` fork.
//!
//! Applications draw larger when the compositor they are on scales its own
//! output. Scaling pixelflux's capture output instead shrinks the logical size
//! the session is handed, which upscales the desktop rather than enlarging its
//! interface, so a DPI change for a nested session lands here.
//!
//! A session that opens one screen per capture output (a wlroots compositor
//! under `WLR_WL_OUTPUTS`) arranges them by its own rule — side by side in the
//! order they were opened — and that layout, not the capture one, is what
//! places its windows and carries its pointer between screens. So the capture
//! arrangement is mirrored into it (`set_screen_layout`), read back
//! (`list_screens`), and the screens no capture output drives yet are held at a
//! token size (`hold_spare_screens`) so the desktop does not lay out across a
//! screen nobody sees. Screens are addressed by position in name order
//! (WL-1, WL-2, ...), the order the session opened them in.
//!
//! Compositors without the protocol report [`ScaleOutcome::Unsupported`] and
//! position nothing here; their scale comes from the capture output, which
//! they follow, and a KWin session's arrangement lands through `kdeclient`
//! (`kde_output_management_v2`) instead. Blocking, off the compositor thread,
//! with every round-trip deadline-bounded.

use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};
use wayland_protocols_wlr::output_management::v1::client::{
    zwlr_output_configuration_head_v1::ZwlrOutputConfigurationHeadV1,
    zwlr_output_configuration_v1::{self, ZwlrOutputConfigurationV1},
    zwlr_output_head_v1::{self, ZwlrOutputHeadV1},
    zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
    zwlr_output_mode_v1::{self, ZwlrOutputModeV1},
};

use crate::wayland::wlclient::{IO_TIMEOUT, SyncState, bounded_roundtrip, impl_sync_callback};

/// What a scale request did, from the caller's point of view.
pub enum ScaleOutcome {
    Applied,
    /// The compositor manages no outputs for clients: scale it another way.
    Unsupported,
}

/// An announced head with its name, enabled state, layout position, and scale.
struct Head {
    head: ZwlrOutputHeadV1,
    name: Option<String>,
    enabled: bool,
    pos: (i32, i32),
    scale: f64,
}

/// An enabled screen as a plan sees it: its head, logical position, current mode,
/// and scale.
#[derive(Clone)]
struct Screen {
    head: ZwlrOutputHeadV1,
    pos: (i32, i32),
    mode: (i32, i32),
    scale: f64,
}

/// The logical size wlroots gives a mode at a scale (each axis truncated).
fn logical_size(mode: (i32, i32), scale: f64) -> (i32, i32) {
    (
        (mode.0 as f64 / scale) as i32,
        (mode.1 as f64 / scale) as i32,
    )
}

/// Rectangles arranged in one space, each about to take a new size: the same
/// arrangement in the space of the new sizes. One whose edge met another's far
/// edge meets it still; every other coordinate is kept. A session places its
/// screens in its logical space, where a scaled screen is smaller than its mode,
/// so an arrangement stated in capture pixels closes up around it here, and a
/// screen changing scale carries its neighbors with it.
pub(crate) fn close_gaps(
    rects: &[(i32, i32, i32, i32)],
    sizes: &[(i32, i32)],
) -> Vec<(i32, i32, i32, i32)> {
    let mut out: Vec<(i32, i32, i32, i32)> = rects
        .iter()
        .zip(sizes)
        .map(|(r, s)| (r.0, r.1, s.0, s.1))
        .collect();
    // Each axis in the order of the old coordinate, so a neighbor is placed
    // before the rectangle that follows it.
    let mut order: Vec<usize> = (0..out.len()).collect();
    order.sort_by_key(|&i| rects[i].0);
    for &i in &order {
        if let Some(&j) = order
            .iter()
            .find(|&&j| rects[j].0 < rects[i].0 && rects[j].0 + rects[j].2 == rects[i].0)
        {
            out[i].0 = out[j].0 + out[j].2;
        }
    }
    order.sort_by_key(|&i| rects[i].1);
    for &i in &order {
        if let Some(&j) = order
            .iter()
            .find(|&&j| rects[j].1 < rects[i].1 && rects[j].1 + rects[j].3 == rects[i].1)
        {
            out[i].1 = out[j].1 + out[j].3;
        }
    }
    out
}

#[derive(Default)]
struct OutState {
    manager: Option<ZwlrOutputManagerV1>,
    /// The manager's order is its own; screens are addressed by name below.
    heads: Vec<Head>,
    /// Size per announced mode object, and the mode each head currently holds:
    /// a head carries no size of its own, so the two are joined to report one.
    modes: Vec<(ZwlrOutputModeV1, (i32, i32))>,
    current: Vec<(ZwlrOutputHeadV1, ZwlrOutputModeV1)>,
    serial: Option<u32>,
    applied: Option<bool>,
    /// The refusal was a cancellation: the configuration went stale under a
    /// state change of the compositor's own, so the same plan on a fresh
    /// serial can still land.
    canceled: bool,
    sync_done: bool,
}

impl SyncState for OutState {
    fn sync_done_mut(&mut self) -> &mut bool {
        &mut self.sync_done
    }
}

impl OutState {
    /// The size of the mode a head holds; `(0, 0)` where it announced none.
    fn mode_size(&self, head: &ZwlrOutputHeadV1) -> (i32, i32) {
        self.current
            .iter()
            .find(|(h, _)| h == head)
            .and_then(|(_, mode)| self.modes.iter().find(|(m, _)| m == mode))
            .map(|(_, size)| *size)
            .unwrap_or((0, 0))
    }

    /// The enabled screens in the order their names put them (WL-1, WL-2, ...),
    /// which is the order a session opens them in and so the order they map to
    /// displays; the manager's own announcement order carries no such promise.
    fn screens(&self) -> Vec<Screen> {
        let mut screens: Vec<(String, Screen)> = self
            .heads
            .iter()
            .filter(|h| h.enabled)
            .map(|h| {
                let screen = Screen {
                    head: h.head.clone(),
                    pos: h.pos,
                    mode: self.mode_size(&h.head),
                    scale: h.scale,
                };
                (h.name.clone().unwrap_or_default(), screen)
            })
            .collect();
        screens.sort_by_key(|(name, _)| (trailing_number(name), name.clone()));
        screens.into_iter().map(|(_, s)| s).collect()
    }
}
impl_sync_callback!(OutState);

impl Dispatch<wl_registry::WlRegistry, ()> for OutState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
            && interface == "zwlr_output_manager_v1"
            && state.manager.is_none()
        {
            state.manager = Some(registry.bind(name, version.min(4), qh, ()));
        }
    }
}

impl Dispatch<ZwlrOutputManagerV1, ()> for OutState {
    fn event(
        state: &mut Self,
        _: &ZwlrOutputManagerV1,
        event: zwlr_output_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_output_manager_v1::Event::Head { head } => state.heads.push(Head {
                head,
                name: None,
                enabled: false,
                pos: (0, 0),
                scale: 1.0,
            }),
            // Every configuration is built against the serial of the state it
            // was read from; a stale one is refused by the compositor.
            zwlr_output_manager_v1::Event::Done { serial } => state.serial = Some(serial),
            _ => {}
        }
    }

    wayland_client::event_created_child!(OutState, ZwlrOutputManagerV1, [
        zwlr_output_manager_v1::EVT_HEAD_OPCODE => (ZwlrOutputHeadV1, ()),
    ]);
}

impl Dispatch<ZwlrOutputHeadV1, ()> for OutState {
    fn event(
        state: &mut Self,
        head: &ZwlrOutputHeadV1,
        event: zwlr_output_head_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(entry) = state.heads.iter_mut().find(|h| h.head == *head) else {
            return;
        };
        match event {
            zwlr_output_head_v1::Event::Name { name } => entry.name = Some(name),
            zwlr_output_head_v1::Event::Enabled { enabled } => entry.enabled = enabled != 0,
            zwlr_output_head_v1::Event::Position { x, y } => entry.pos = (x, y),
            zwlr_output_head_v1::Event::Scale { scale } => entry.scale = scale,
            zwlr_output_head_v1::Event::CurrentMode { mode } => {
                let head = head.clone();
                state.current.retain(|(h, _)| *h != head);
                state.current.push((head, mode));
            }
            _ => {}
        }
    }

    wayland_client::event_created_child!(OutState, ZwlrOutputHeadV1, [
        zwlr_output_head_v1::EVT_MODE_OPCODE => (ZwlrOutputModeV1, ()),
    ]);
}

impl Dispatch<ZwlrOutputConfigurationV1, ()> for OutState {
    fn event(
        state: &mut Self,
        _: &ZwlrOutputConfigurationV1,
        event: zwlr_output_configuration_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_output_configuration_v1::Event::Succeeded => state.applied = Some(true),
            zwlr_output_configuration_v1::Event::Failed => state.applied = Some(false),
            zwlr_output_configuration_v1::Event::Cancelled => {
                state.canceled = true;
                state.applied = Some(false);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrOutputModeV1, ()> for OutState {
    fn event(
        state: &mut Self,
        mode: &ZwlrOutputModeV1,
        event: zwlr_output_mode_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_output_mode_v1::Event::Size { width, height } = event {
            let mode = mode.clone();
            state.modes.retain(|(m, _)| *m != mode);
            state.modes.push((mode, (width, height)));
        }
    }
}
delegate_noop!(OutState: ZwlrOutputConfigurationHeadV1);

/// Set the scale of the `index`-th screen of the compositor on `socket_path`,
/// leaving its mode and position alone. Blocking; call off the compositor's
/// calloop thread.
pub fn set_output_scale(
    socket_path: &str,
    index: usize,
    scale: f64,
) -> Result<ScaleOutcome, String> {
    if !(0.1..=16.0).contains(&scale) {
        return Err(format!("scale {scale} out of range"));
    }
    configure(socket_path, |screens| rescale(screens, index, None, scale)).map(|changed| {
        if changed == 0 {
            ScaleOutcome::Unsupported
        } else {
            ScaleOutcome::Applied
        }
    })
}

/// The plan that gives screen `index` a scale, and the mode asked for, and
/// closes the others up around the logical size it takes.
fn rescale(
    screens: &[Screen],
    index: usize,
    mode: Option<(i32, i32)>,
    scale: f64,
) -> Result<Vec<(ZwlrOutputHeadV1, Plan)>, String> {
    if index >= screens.len() {
        return Err(format!("no enabled screen at index {index}"));
    }
    let rects: Vec<(i32, i32, i32, i32)> = screens
        .iter()
        .map(|s| {
            let (w, h) = logical_size(s.mode, s.scale);
            (s.pos.0, s.pos.1, w, h)
        })
        .collect();
    let mut sizes: Vec<(i32, i32)> = rects.iter().map(|r| (r.2, r.3)).collect();
    sizes[index] = logical_size(mode.unwrap_or(screens[index].mode), scale);
    let placed = close_gaps(&rects, &sizes);
    Ok(screens
        .iter()
        .enumerate()
        .filter_map(|(i, s)| {
            let position = (placed[i].0, placed[i].1);
            let mut plan = Plan {
                position: (position != s.pos).then_some(position),
                ..Plan::default()
            };
            if i == index {
                plan.mode = mode;
                plan.scale = Some(scale);
            }
            (i == index || plan.position.is_some()).then(|| (s.head.clone(), plan))
        })
        .collect())
}

/// Give the `index`-th screen of the compositor on `socket_path` this mode and
/// scale in one configuration.
///
/// A session lays its desktop out once per applied configuration, so setting the
/// two separately leaves it briefly at a geometry that never exists — a screen
/// still carrying the pre-connect mode at the new scale is a fraction of its
/// final size, and a client that does not lay out again keeps that size.
pub fn set_screen_geometry(
    socket_path: &str,
    index: usize,
    size: (i32, i32),
    scale: f64,
) -> Result<ScaleOutcome, String> {
    if !(0.1..=16.0).contains(&scale) {
        return Err(format!("scale {scale} out of range"));
    }
    if size.0 <= 0 || size.1 <= 0 {
        return Err(format!("size {}x{} out of range", size.0, size.1));
    }
    configure(socket_path, move |screens| {
        rescale(screens, index, Some(size), scale)
    })
    .map(|changed| {
        if changed == 0 {
            ScaleOutcome::Unsupported
        } else {
            ScaleOutcome::Applied
        }
    })
}

/// The session's enabled screens as `(name, x, y, width, height)`, in screen
/// order — what the compositor actually did with a layout and a mode, which is
/// not always what it was asked for. A head carries no size itself, so the size
/// is its current mode's; `(0, 0)` where it announced none. Empty when the
/// compositor manages no outputs for clients.
pub fn list_screens(socket_path: &str) -> Result<Vec<super::AppScreen>, String> {
    let stream =
        UnixStream::connect(socket_path).map_err(|e| format!("connect {socket_path}: {e}"))?;
    let conn = Connection::from_socket(stream).map_err(|e| format!("wayland setup: {e}"))?;
    let mut queue: EventQueue<OutState> = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut state = OutState::default();
    bounded_roundtrip(&conn, &mut queue, &mut state)?;
    let Some(manager) = state.manager.clone() else {
        return Ok(Vec::new());
    };
    bounded_roundtrip(&conn, &mut queue, &mut state)?;
    let mut screens: Vec<(String, i32, i32, i32, i32)> = state
        .heads
        .iter()
        .filter(|h| h.enabled)
        .map(|h| {
            let (w, hgt) = state.mode_size(&h.head);
            (h.name.clone().unwrap_or_default(), h.pos.0, h.pos.1, w, hgt)
        })
        .collect();
    screens.sort_by_key(|(name, _, _, _, _)| (trailing_number(name), name.clone()));
    manager.stop();
    let _ = queue.flush();
    Ok(screens)
}

/// Lay the session's screens out at `rects`, one `(x, y, width, height)` per
/// screen in screen order, so its own layout matches the arrangement the capture
/// outputs were placed in.
///
/// A session compositor arranges the screens it opens by its own rule — wlroots
/// puts them side by side in the order they appear — and that layout, not the
/// capture one, is what places windows and carries the pointer between screens.
/// Every screen is positioned in one configuration: applied one at a time, an
/// intermediate state overlaps two screens and the compositor reflows around it.
/// The rectangles are capture pixels; the positions land in the session's logical
/// space, where a scaled screen is its mode over its scale, closed up so a screen
/// beside a scaled one meets its logical edge rather than its pixel edge.
///
/// Returns how many screens were positioned; 0 when the compositor manages no
/// outputs for clients, which is where a session on KWin lands (it offers
/// `kde_output_management_v2` instead) and where the arrangement stays whatever
/// that compositor chose.
pub fn set_screen_layout(
    socket_path: &str,
    rects: Vec<(i32, i32, i32, i32)>,
) -> Result<usize, String> {
    if rects.iter().any(|(_, _, w, h)| *w <= 0 || *h <= 0) {
        return Err("a screen rectangle has a non-positive size".to_string());
    }
    configure(socket_path, move |screens| {
        let n = screens.len().min(rects.len());
        let sizes: Vec<(i32, i32)> = screens
            .iter()
            .zip(&rects)
            .map(|(s, r)| logical_size((r.2, r.3), s.scale))
            .collect();
        let placed = close_gaps(&rects[..n], &sizes);
        Ok(screens
            .iter()
            .zip(&rects)
            .zip(placed)
            .map(|((s, r), p)| {
                (
                    s.head.clone(),
                    Plan {
                        mode: Some((r.2, r.3)),
                        position: Some((p.0, p.1)),
                        ..Plan::default()
                    },
                )
            })
            .collect())
    })
}

/// Hold every screen past the first `keep` at `size`. A session compositor opens
/// the screens it was started with whether or not anything watches them, and one
/// held at a real screen's size stretches the session's coordinate space onto a
/// screen nobody sees. Returns how many were resized.
pub fn hold_spare_screens(
    socket_path: &str,
    keep: usize,
    size: (i32, i32),
) -> Result<usize, String> {
    configure(socket_path, move |screens| {
        Ok(screens
            .iter()
            .skip(keep)
            .map(|s| {
                (
                    s.head.clone(),
                    Plan {
                        mode: Some(size),
                        ..Plan::default()
                    },
                )
            })
            .collect())
    })
}

/// The number a screen's name ends in (WL-2 -> 2), or none, which sorts first.
pub(crate) fn trailing_number(name: &str) -> u32 {
    let digits: String = name
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits
        .chars()
        .rev()
        .collect::<String>()
        .parse()
        .unwrap_or(0)
}

/// What a head is being asked to change; an unset field keeps its current value.
#[derive(Clone, Copy, Default)]
struct Plan {
    mode: Option<(i32, i32)>,
    scale: Option<f64>,
    position: Option<(i32, i32)>,
}

/// Why a configuration did not land.
enum ConfigErr {
    /// The compositor canceled it, which says only that its own state moved
    /// while the configuration was alive. The serial is what went stale, not
    /// the plan.
    Stale,
    Other(String),
}

impl From<String> for ConfigErr {
    fn from(e: String) -> Self {
        ConfigErr::Other(e)
    }
}

/// Rebuilds a canceled configuration until this much time is spent. A screen
/// arriving or leaving cancels whatever is in flight, and that is the same
/// moment a display's mode and scale are being applied. Cancellations arrive in
/// bursts, so the budget is a duration rather than a count of attempts: a count
/// that outlasts a burst on one machine is spent inside one on a busier machine,
/// where the attempts slow down and the burst does not. It can be generous
/// because the caller is off the event loop with the GIL released, while giving
/// up leaves the display on a capture-side scale until something asks again.
const CONFIGURE_BUDGET: Duration = Duration::from_secs(1);

/// Apply `plan` to the compositor's enabled screens, retrying while the
/// compositor cancels. `plan` receives them in screen order and answers with
/// the heads to change; heads it leaves out keep their configuration. It is
/// called once per attempt, against the screens of that attempt. 0 = the
/// compositor manages no outputs, or the plan asked for nothing.
fn configure<F>(socket_path: &str, plan: F) -> Result<usize, String>
where
    F: Fn(&[Screen]) -> Result<Vec<(ZwlrOutputHeadV1, Plan)>, String>,
{
    let deadline = Instant::now() + CONFIGURE_BUDGET;
    let mut stale = 0;
    loop {
        match configure_once(socket_path, &plan) {
            Ok(applied) => return Ok(applied),
            Err(ConfigErr::Other(e)) => return Err(e),
            Err(ConfigErr::Stale) => stale += 1,
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "the compositor canceled the configuration {stale} times"
            ));
        }
    }
}

/// One attempt: a connection of its own, the heads and the serial that
/// stamps them, and `plan` applied against that state. A retry reconnects
/// rather than waiting for a fresh serial on the one it has, because the change
/// that canceled it is often a head arriving or leaving, and the heads a kept
/// connection holds do not survive that.
fn configure_once<F>(socket_path: &str, plan: &F) -> Result<usize, ConfigErr>
where
    F: Fn(&[Screen]) -> Result<Vec<(ZwlrOutputHeadV1, Plan)>, String>,
{
    let stream =
        UnixStream::connect(socket_path).map_err(|e| format!("connect {socket_path}: {e}"))?;
    let conn = Connection::from_socket(stream).map_err(|e| format!("wayland setup: {e}"))?;
    let mut queue: EventQueue<OutState> = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut state = OutState::default();
    bounded_roundtrip(&conn, &mut queue, &mut state)?;
    let Some(manager) = state.manager.clone() else {
        return Ok(0);
    };
    // The heads and the serial that stamps them arrive after the bind.
    bounded_roundtrip(&conn, &mut queue, &mut state)?;
    let serial = state
        .serial
        .ok_or_else(|| "output manager sent no state serial".to_string())?;
    let screens = state.screens();
    let wanted = plan(&screens)?;
    if wanted.is_empty() {
        manager.stop();
        let _ = queue.flush();
        return Ok(0);
    }

    // A configuration describes every head: one left out would be disabled.
    let config = manager.create_configuration(serial, &qh, ());
    for screen in &screens {
        let cfg_head = config.enable_head(&screen.head, &qh, ());
        if let Some((_, want)) = wanted.iter().find(|(h, _)| *h == screen.head) {
            if let Some((w, h)) = want.mode {
                cfg_head.set_custom_mode(w, h, 0);
            }
            if let Some(scale) = want.scale {
                cfg_head.set_scale(scale);
            }
            if let Some((x, y)) = want.position {
                cfg_head.set_position(x, y);
            }
        }
    }
    config.apply();
    queue
        .flush()
        .map_err(|e| format!("flush configuration: {e}"))?;
    state.applied = None;
    let deadline = Instant::now() + IO_TIMEOUT;
    while state.applied.is_none() && Instant::now() < deadline {
        bounded_roundtrip(&conn, &mut queue, &mut state)?;
    }
    config.destroy();
    manager.stop();
    let _ = queue.flush();
    match state.applied {
        Some(true) => Ok(wanted.len()),
        _ if state.canceled => Err(ConfigErr::Stale),
        _ => Err(ConfigErr::Other(
            "the compositor refused the configuration".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::close_gaps;

    /// The arrangement of the capture outputs, in pixels, becomes the session's
    /// logical one: a screen at scale 2 is half its mode there, and whichever
    /// screen sat against it closes up to that edge, on either axis and down a
    /// chain; nothing moves at scale 1.
    #[test]
    fn scaled_screens_close_up() {
        let primary_right = [(3024, 0, 1920, 992), (0, 0, 3024, 1764)];
        assert_eq!(
            close_gaps(&primary_right, &[(1920, 992), (1512, 882)]),
            vec![(1512, 0, 1920, 992), (0, 0, 1512, 882)]
        );
        let primary_left = [(0, 0, 1920, 1080), (1920, 0, 1920, 1080)];
        assert_eq!(
            close_gaps(&primary_left, &[(960, 540), (1920, 1080)]),
            vec![(0, 0, 960, 540), (960, 0, 1920, 1080)]
        );
        assert_eq!(
            close_gaps(&primary_left, &[(1920, 1080), (1920, 1080)]),
            primary_left.to_vec()
        );
        let below = [(0, 1764, 1920, 992), (0, 0, 3024, 1764)];
        assert_eq!(
            close_gaps(&below, &[(1920, 992), (1512, 882)]),
            vec![(0, 882, 1920, 992), (0, 0, 1512, 882)]
        );
        let chain = [
            (0, 0, 1000, 500),
            (1000, 0, 1000, 500),
            (2000, 0, 1000, 500),
        ];
        assert_eq!(
            close_gaps(&chain, &[(500, 250), (500, 250), (1000, 500)]),
            vec![(0, 0, 500, 250), (500, 0, 500, 250), (1000, 0, 1000, 500)]
        );
    }
}
