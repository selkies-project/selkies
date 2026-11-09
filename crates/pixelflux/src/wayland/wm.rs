/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The window policy's seat in the compositor: what the frontend tells an installed
//! [`WindowPolicy`] and how its commands become Smithay state.
//!
//! With no policy installed every call here is a no-op and the frontend keeps its own behavior.
//! Once one is (`Compositor::install_window_policy`), the frontend asks it at each decision
//! point through the functions below instead of forcing a window fullscreen, and this module
//! carries the answers out: a configure becomes an xdg configure and a Space mapping, a
//! stacking order a sequence of raises, a frame a set of memory render buffers drawn with the
//! window, an overlay one drawn over every output, an effect a transform the render tick
//! applies, a hidden window a Space unmapping that keeps the window for its return.
//!
//! The policy runs inside the compositor's handlers, so a panic in it would end the stream;
//! [`dispatch`] catches it, drops the policy, and puts every window back the way the frontend
//! shows them without one, logging the panic once, so a window-manager bug degrades the
//! desktop rather than ending the stream.
//!
//! Window ids are the frontend's `WindowMeta` ids, the ones the window list reports, so the
//! policy and the consumer name the same window the same way.

use std::collections::{BTreeMap, HashMap};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Instant;

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::memory::{
    MemoryRenderBuffer, MemoryRenderBufferRenderElement,
};
use smithay::backend::renderer::{ImportMem, Renderer, Texture};
use smithay::desktop::{Window, layer_map_for_output};
use smithay::input::keyboard::ModifiersState;
use smithay::input::pointer::{CursorIcon, CursorImageStatus};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State as XdgState;
use smithay::reexports::wayland_server::Resource;
use smithay::utils::{Logical, Physical, Point, Rectangle, Scale, Transform as OutputTransform};
use smithay::wayland::compositor::with_states;
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::shell::wlr_layer::Layer;
use smithay::wayland::shell::xdg::SurfaceCachedState;
use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;

use crate::wayland::frontend::{
    AppState, FocusTarget, next_serial, wayland_time, window_meta, window_output_id,
};
use crate::wm::{
    Command, Decoration, Event, Extents, Frame, Image, KeyEvent, Modifiers, OutputInfo,
    PointerEvent, PointerKind, Rect, Request, Size, States, Transform, Under, Verdict, WindowId,
    WindowInfo, WindowKind, WindowPolicy, WindowType,
};

/// A frame's parts as render buffers, placed relative to the client area.
pub struct FrameBuffers {
    pub extents: Extents,
    pub parts: Vec<(Rect, MemoryRenderBuffer)>,
}

/// An overlay as a render buffer at a layout rectangle.
pub struct Overlay {
    pub rect: Rect,
    pub buffer: MemoryRenderBuffer,
}

/// Everything the compositor keeps for an installed policy.
pub struct WmState {
    policy: Option<Box<dyn WindowPolicy>>,
    pub frames: HashMap<WindowId, FrameBuffers>,
    pub overlays: BTreeMap<u32, Overlay>,
    pub effects: HashMap<WindowId, Transform>,
    /// Windows the policy hid (minimized, or on another desktop): unmapped from the Space,
    /// kept here for their return.
    pub hidden: HashMap<WindowId, Window>,
    /// The client rectangle the policy last configured each window to.
    pub rects: HashMap<WindowId, Rect>,
    /// The states the policy last configured each window to.
    pub states: HashMap<WindowId, States>,
    /// What each client said about decorations.
    pub decorations: HashMap<WindowId, Decoration>,
    /// The windows the policy was told about, with the size it last saw them at.
    pub known: HashMap<WindowId, Size>,
    /// Where a toplevel goes once it commits the size it was configured with along with the
    /// move: a resize from a window's top or left edge, or a maximize, changes both, and moving
    /// the old buffer first would show its far edge jump and come back.
    pub pending_moves: HashMap<WindowId, (i32, i32)>,
    /// The size of the configure each toplevel last acknowledged, as of its last commit, so a
    /// commit that answers a configure is told from one that was already on its way.
    pub acked: HashMap<WindowId, Option<Size>>,
    /// When the policy asked to be ticked.
    pub wake_at: Option<Instant>,
    /// The cursor the policy shows while it holds the pointer; `None` leaves the client's.
    pub cursor: Option<CursorIcon>,
    /// Whether the policy took the last pointer event, so the clients saw the pointer leave.
    pub pointer_taken: bool,
    /// The outputs the policy was last told about.
    pub outputs: Vec<OutputInfo>,
    /// The focus the policy last asked for.
    pub focus: Option<WindowId>,
    /// Something the policy presents changed since the last render.
    pub dirty: bool,
}

impl WmState {
    fn new(policy: Box<dyn WindowPolicy>) -> WmState {
        WmState {
            policy: Some(policy),
            frames: HashMap::new(),
            overlays: BTreeMap::new(),
            effects: HashMap::new(),
            hidden: HashMap::new(),
            rects: HashMap::new(),
            states: HashMap::new(),
            decorations: HashMap::new(),
            known: HashMap::new(),
            pending_moves: HashMap::new(),
            acked: HashMap::new(),
            wake_at: None,
            cursor: None,
            pointer_taken: false,
            outputs: Vec::new(),
            focus: None,
            dirty: false,
        }
    }
}

/// Whether a policy is installed and alive.
pub fn active(state: &AppState) -> bool {
    state.wm.as_ref().is_some_and(|wm| wm.policy.is_some())
}

/// Install a policy: it is told the outputs and every window already shown, so a window
/// configured fullscreen before it came is placed the way it would have been.
pub fn install(state: &mut AppState, policy: Box<dyn WindowPolicy>) {
    if let Some(old) = state.wm.take() {
        drop(old);
    }
    state.wm = Some(WmState::new(policy));
    println!("[Wayland] Window policy installed: the compositor manages its windows.");
    outputs_changed(state);
    let windows: Vec<Window> = state.space.elements().cloned().collect();
    for window in windows {
        let Some(meta) = window_meta(&window) else {
            continue;
        };
        if meta.parked.load(std::sync::atomic::Ordering::Relaxed) {
            continue;
        }
        window_added(state, &window);
        let size = window.geometry().size;
        window_mapped(
            state,
            &window,
            Size {
                w: size.w,
                h: size.h,
            },
        );
    }
}

/// The policy's decision about an event; `Pass` with no policy.
pub fn dispatch(state: &mut AppState, event: Event) -> Verdict {
    let Some(wm) = state.wm.as_mut() else {
        return Verdict::Pass;
    };
    let Some(policy) = wm.policy.as_mut() else {
        return Verdict::Pass;
    };
    let mut out = Vec::new();
    let result = catch_unwind(AssertUnwindSafe(|| policy.handle(event, &mut out)));
    match result {
        Ok(verdict) => {
            for command in out {
                apply(state, command);
            }
            verdict
        }
        Err(payload) => {
            let what = payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "no message".to_string());
            eprintln!(
                "[Wayland] The window policy panicked ({what}); windows are shown without it from here on."
            );
            uninstall(state);
            Verdict::Pass
        }
    }
}

/// Drop the policy and show every window the way the frontend does without one.
fn uninstall(state: &mut AppState) {
    let Some(mut wm) = state.wm.take() else {
        return;
    };
    wm.policy = None;
    let hidden: Vec<Window> = wm.hidden.drain().map(|(_, w)| w).collect();
    for window in hidden {
        let output = window_output_id(&window);
        state.place_window_on_output(&window, output);
    }
    let windows: Vec<Window> = state.space.elements().cloned().collect();
    for window in windows {
        let output = window_output_id(&window);
        state.place_window_on_output(&window, output);
    }
    for node in &mut state.output_nodes {
        if let Some(cap) = node.capture.as_mut() {
            cap.needs_full_render = true;
            cap.change_pending = true;
        }
    }
    state.wm = None;
}

/// Every window the compositor has by id: shown, pending, or hidden.
pub fn find_window(state: &AppState, id: WindowId) -> Option<Window> {
    state
        .space
        .elements()
        .chain(state.pending_windows.iter())
        .find(|w| window_meta(w).is_some_and(|m| m.id == id))
        .cloned()
        .or_else(|| state.wm.as_ref().and_then(|wm| wm.hidden.get(&id).cloned()))
        .or_else(|| state.xwm.windows.get(&id).cloned())
}

fn window_id(window: &Window) -> Option<WindowId> {
    window_meta(window).map(|m| m.id)
}

fn window_id_of_surface(
    state: &AppState,
    surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
) -> Option<WindowId> {
    state
        .space
        .elements()
        .chain(state.pending_windows.iter())
        .chain(state.wm.iter().flat_map(|wm| wm.hidden.values()))
        .find(|w| w.wl_surface().as_deref() == Some(surface))
        .and_then(window_id)
}

/// What the policy is told about a window.
pub fn window_info(state: &AppState, window: &Window) -> WindowInfo {
    let id = window_id(window).unwrap_or(0);
    let output = window_output_id(window);
    // An X11 client without Motif hints leaves decorations to the manager; an xdg toplevel
    // that never created a decoration object (GTK 3) draws its own, as labwc treats it.
    let decoration = state
        .wm
        .as_ref()
        .and_then(|wm| wm.decorations.get(&id).copied())
        .unwrap_or(if window.x11_surface().is_some() {
            Decoration::Unspecified
        } else {
            Decoration::Client
        });
    if let Some(surface) = window.x11_surface() {
        return crate::wayland::xwm::window_info(state, id, surface, output, decoration);
    }
    let Some(toplevel) = window.toplevel() else {
        return WindowInfo {
            id,
            output,
            ..WindowInfo::default()
        };
    };
    let (title, app_id, min, max, parent) = with_states(toplevel.wl_surface(), |states| {
        let (min, max) = {
            let mut cached = states.cached_state.get::<SurfaceCachedState>();
            let current = cached.current();
            (current.min_size, current.max_size)
        };
        let attributes = states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .map(|d| d.lock().unwrap());
        match attributes {
            Some(a) => (
                a.title.clone().unwrap_or_default(),
                a.app_id.clone().unwrap_or_default(),
                min,
                max,
                a.parent.clone(),
            ),
            None => (String::new(), String::new(), min, max, None),
        }
    });
    let parent = parent.and_then(|p| window_id_of_surface(state, &p));
    let (wants_fullscreen, wants_maximized) =
        toplevel.with_committed_state(|committed| match committed {
            Some(c) => (
                c.states.contains(XdgState::Fullscreen),
                c.states.contains(XdgState::Maximized),
            ),
            None => (false, false),
        });
    WindowInfo {
        id,
        kind: Some(WindowKind::Xdg),
        window_type: if parent.is_some() {
            WindowType::Dialog
        } else {
            WindowType::Normal
        },
        title,
        app_id,
        instance: String::new(),
        parent,
        modal: false,
        hints: crate::wm::SizeHints {
            min: (min.w > 0 || min.h > 0).then_some(Size { w: min.w, h: min.h }),
            max: (max.w > 0 || max.h > 0).then_some(Size { w: max.w, h: max.h }),
            increment: None,
            base: None,
        },
        requested: None,
        positioned: false,
        decoration,
        output,
        pid: toplevel
            .wl_surface()
            .client()
            .and_then(|c| c.get_credentials(&state.dh).ok())
            .map(|c| c.pid as u32),
        wants_fullscreen,
        wants_maximized,
        wants_minimized: false,
        skip_taskbar: false,
        accepts_focus: true,
    }
}

/// Tell the policy about a window waiting for its first configure.
pub fn window_added(state: &mut AppState, window: &Window) {
    let Some(id) = window_id(window) else {
        return;
    };
    if !active(state) {
        return;
    }
    let info = window_info(state, window);
    if let Some(wm) = state.wm.as_mut() {
        wm.known.insert(id, Size::default());
    }
    dispatch(state, Event::Added(info));
}

/// Tell the policy a window's details changed.
pub fn window_changed(state: &mut AppState, window: &Window) {
    let Some(id) = window_id(window) else {
        return;
    };
    if !active(state)
        || !state
            .wm
            .as_ref()
            .is_some_and(|wm| wm.known.contains_key(&id))
    {
        return;
    }
    let info = window_info(state, window);
    dispatch(state, Event::Changed(info));
}

/// The window drew its first buffer: it is on screen at this size.
pub fn window_mapped(state: &mut AppState, window: &Window, size: Size) {
    let Some(id) = window_id(window) else {
        return;
    };
    if !active(state) {
        return;
    }
    if !state
        .wm
        .as_ref()
        .is_some_and(|wm| wm.known.contains_key(&id))
    {
        window_added(state, window);
    }
    if let Some(wm) = state.wm.as_mut() {
        wm.known.insert(id, size);
    }
    dispatch(state, Event::Mapped { id, size });
    mark_dirty(state);
}

/// A mapped window committed a size. The policy is told when it differs from the last, and
/// also when the commit answers a configure with a size other than the one asked for, which
/// a client snapping to its cell size does without changing size at all; a move held for the
/// configure is carried out on that answer, with whatever size the client chose.
pub fn window_resized(state: &mut AppState, window: &Window, size: Size) {
    let Some(id) = window_id(window) else {
        return;
    };
    let acked = window
        .toplevel()
        .and_then(|toplevel| {
            toplevel.with_committed_state(|committed| committed.and_then(|c| c.size))
        })
        .map(|s| Size { w: s.w, h: s.h });
    let Some(wm) = state.wm.as_mut() else {
        return;
    };
    let Some(last) = wm.known.get_mut(&id) else {
        return;
    };
    let changed = *last != size;
    *last = size;
    let acked_now = acked.is_some() && wm.acked.insert(id, acked) != Some(acked);
    let requested = wm.rects.get(&id).map(|r| Size { w: r.w, h: r.h });
    let answered = acked_now && requested.is_some_and(|r| r != size);
    let pending = if changed || acked_now {
        wm.pending_moves.remove(&id)
    } else {
        None
    };
    if let Some(location) = pending {
        state.space.map_element(window.clone(), location, false);
        let rect = Rect::new(location.0, location.1, size.w, size.h);
        move_to_output_under(state, window, rect);
        state.refocus_pointer();
        mark_dirty(state);
    }
    if (changed || answered) && active(state) {
        dispatch(state, Event::Resized { id, size });
    }
}

/// The client withdrew its window.
pub fn window_unmapped(state: &mut AppState, window: &Window) {
    let Some(id) = window_id(window) else {
        return;
    };
    if let Some(wm) = state.wm.as_mut() {
        wm.frames.remove(&id);
        wm.effects.remove(&id);
        wm.hidden.remove(&id);
    }
    if active(state)
        && state
            .wm
            .as_ref()
            .is_some_and(|wm| wm.known.contains_key(&id))
    {
        dispatch(state, Event::Unmapped { id });
        mark_dirty(state);
    }
}

/// The window is gone.
pub fn window_removed(state: &mut AppState, id: WindowId) {
    let known = state
        .wm
        .as_ref()
        .is_some_and(|wm| wm.known.contains_key(&id));
    if let Some(wm) = state.wm.as_mut() {
        wm.frames.remove(&id);
        wm.effects.remove(&id);
        wm.hidden.remove(&id);
        wm.rects.remove(&id);
        wm.states.remove(&id);
        wm.decorations.remove(&id);
        wm.known.remove(&id);
        wm.pending_moves.remove(&id);
        wm.acked.remove(&id);
        if wm.focus == Some(id) {
            wm.focus = None;
        }
    }
    if known && active(state) {
        dispatch(state, Event::Removed { id });
        mark_dirty(state);
    }
}

/// A protocol request about a window.
pub fn request(state: &mut AppState, id: WindowId, request: Request) -> bool {
    if !active(state) {
        return false;
    }
    dispatch(state, Event::Request { id, request });
    true
}

/// Record what a client said about who decorates it, telling the policy where it knows the
/// window.
pub fn set_decoration(state: &mut AppState, window: &Window, decoration: Decoration) {
    let Some(id) = window_id(window) else {
        return;
    };
    if let Some(wm) = state.wm.as_mut() {
        wm.decorations.insert(id, decoration);
    }
    window_changed(state, window);
}

/// The policy's decoration choice for a window: whether it draws a frame, so the client is
/// told the matching xdg-decoration mode.
pub fn decorates(state: &AppState, id: WindowId) -> Option<bool> {
    state
        .wm
        .as_ref()
        .filter(|wm| wm.policy.is_some())
        .map(|wm| wm.frames.contains_key(&id))
}

/// Tell the policy the layout: every output with its workarea, when it differs from the last.
pub fn outputs_changed(state: &mut AppState) {
    if !active(state) {
        return;
    }
    let mut outputs = Vec::new();
    for node in state.output_nodes.iter().filter(|n| n.owner.is_none()) {
        let Some(geo) = node.logical_geometry() else {
            continue;
        };
        let zone = layer_map_for_output(&node.output).non_exclusive_zone();
        outputs.push(OutputInfo {
            id: node.id,
            rect: Rect::new(geo.loc.x, geo.loc.y, geo.size.w, geo.size.h),
            workarea: Rect::new(
                geo.loc.x + zone.loc.x,
                geo.loc.y + zone.loc.y,
                zone.size.w,
                zone.size.h,
            ),
            scale: node.output.current_scale().fractional_scale(),
            primary: node.id == 0,
        });
    }
    let same = state.wm.as_ref().is_some_and(|wm| wm.outputs == outputs);
    if same {
        return;
    }
    if let Some(wm) = state.wm.as_mut() {
        wm.outputs = outputs.clone();
    }
    dispatch(state, Event::Outputs(outputs));
    mark_dirty(state);
}

/// A taskbar said where it shows a window.
pub fn taskbar_rect(state: &mut AppState, id: WindowId, rect: Option<Rect>) {
    if active(state) {
        dispatch(state, Event::TaskbarRect { id, rect });
    }
}

/// A shell asked to show or restore the desktop.
pub fn show_desktop(state: &mut AppState, on: bool) {
    if active(state) {
        dispatch(state, Event::ShowDesktop(on));
    }
}

/// A pager asked to switch to a virtual desktop, counted from zero.
pub fn activate_desktop(state: &mut AppState, index: u32) {
    if active(state) {
        dispatch(state, Event::ActivateDesktop(index));
    }
}

/// The modifiers the seat's keyboard holds.
fn modifiers(state: &AppState) -> Modifiers {
    let mods: Option<ModifiersState> = state.seat.get_keyboard().map(|k| k.modifier_state());
    match mods {
        Some(m) => Modifiers {
            shift: m.shift,
            ctrl: m.ctrl,
            alt: m.alt,
            logo: m.logo,
            caps_lock: m.caps_lock,
            num_lock: m.num_lock,
        },
        None => Modifiers::default(),
    }
}

/// What the compositor's hit test found, for the policy.
pub fn under_of(_state: &AppState, target: &Option<(FocusTarget, Point<f64, Logical>)>) -> Under {
    match target {
        Some((FocusTarget::Window(window), _)) => Under {
            window: window_id(window),
            top_layer: false,
            desktop: false,
        },
        Some((FocusTarget::LayerSurface(layer), _)) => Under {
            window: None,
            top_layer: matches!(layer.layer(), Layer::Top | Layer::Overlay),
            desktop: matches!(layer.layer(), Layer::Background | Layer::Bottom),
        },
        Some((FocusTarget::Popup(_), _)) => Under {
            window: None,
            top_layer: true,
            desktop: false,
        },
        None => Under::default(),
    }
}

/// The policy's verdict on a pointer event at a logical layout point. A taken event makes
/// the clients see the pointer leave, so a frame drag never scrolls or clicks what lies under
/// the frame, and still moves the seat's pointer, since a button is judged at the seat's
/// location: a menu whose motion the policy took would otherwise see every click at the spot
/// it opened on.
pub fn pointer(
    state: &mut AppState,
    kind: PointerKind,
    p: Point<f64, Logical>,
    under: Under,
) -> Verdict {
    if !active(state) {
        return Verdict::Pass;
    }
    let event = PointerEvent {
        x: p.x,
        y: p.y,
        kind,
        modifiers: modifiers(state),
        time: wayland_time().millis(),
    };
    let verdict = dispatch(state, Event::Pointer(event, under));
    let taken = verdict == Verdict::Consumed;
    if let Some(wm) = state.wm.as_mut() {
        wm.pointer_taken = taken;
    }
    if taken
        && let Some(pointer) = state.seat.get_pointer()
        && !pointer.is_grabbed()
    {
        pointer.motion(
            state,
            None,
            &smithay::input::pointer::MotionEvent {
                location: p,
                serial: next_serial(),
                time: wayland_time(),
            },
        );
        pointer.frame(state);
    }
    verdict
}

/// The policy's verdict on a key, with the keysyms the seat's keymap resolved.
pub fn key(
    state: &mut AppState,
    keysym: u32,
    base_keysym: u32,
    keycode: u32,
    pressed: bool,
) -> Verdict {
    if !active(state) {
        return Verdict::Pass;
    }
    let event = KeyEvent {
        keysym,
        base_keysym,
        keycode,
        pressed,
        modifiers: modifiers(state),
    };
    dispatch(state, Event::Key(event))
}

/// The keyboard filter the seat's `input` takes: the policy's verdict as an intercept, with the
/// keysyms the seat's keymap resolved for the key.
pub fn key_filter(
    state: &mut AppState,
    handle: smithay::input::keyboard::KeysymHandle<'_>,
    pressed: bool,
) -> smithay::input::keyboard::FilterResult<()> {
    if !active(state) {
        return smithay::input::keyboard::FilterResult::Forward;
    }
    let keysym = handle.modified_sym().raw();
    let base_keysym = handle.raw_syms().first().map(|k| k.raw()).unwrap_or(keysym);
    let keycode = handle.raw_code().raw();
    match key(state, keysym, base_keysym, keycode, pressed) {
        Verdict::Consumed => smithay::input::keyboard::FilterResult::Intercept(()),
        Verdict::Pass => smithay::input::keyboard::FilterResult::Forward,
    }
}

/// Whether the policy asked for a tick by `now`.
pub fn tick_due(state: &AppState, now: Instant) -> bool {
    state
        .wm
        .as_ref()
        .and_then(|wm| wm.wake_at)
        .is_some_and(|at| at <= now)
}

/// When the policy next wants a tick.
pub fn next_wake(state: &AppState) -> Option<Instant> {
    state.wm.as_ref().and_then(|wm| wm.wake_at)
}

/// Tick the policy; whether anything it presents changed.
pub fn tick(state: &mut AppState, now: Instant) -> bool {
    if !active(state) {
        return false;
    }
    if let Some(wm) = state.wm.as_mut() {
        wm.wake_at = None;
    }
    dispatch(state, Event::Tick(now));
    take_dirty(state)
}

/// Note that something the policy presents changed, so the captures render a frame.
pub fn mark_dirty(state: &mut AppState) {
    if let Some(wm) = state.wm.as_mut() {
        wm.dirty = true;
    }
    for node in &mut state.output_nodes {
        if let Some(cap) = node.capture.as_mut() {
            cap.change_pending = true;
        }
    }
}

/// Whether the policy's presentation changed since the last call.
pub fn take_dirty(state: &mut AppState) -> bool {
    state
        .wm
        .as_mut()
        .map(|wm| std::mem::take(&mut wm.dirty))
        .unwrap_or(false)
}

/// Tell the policy an X server is up for its X11 clients.
pub fn x11_display(state: &mut AppState, display: u32) {
    if active(state) {
        dispatch(state, Event::X11Display(display));
    }
}

fn buffer_of(image: &Image) -> MemoryRenderBuffer {
    MemoryRenderBuffer::from_slice(
        &image.rgba,
        Fourcc::Abgr8888,
        (image.width as i32, image.height as i32),
        image.scale.max(1),
        OutputTransform::Normal,
        None,
    )
}

/// Carry one command out.
fn apply(state: &mut AppState, command: Command) {
    match command {
        Command::Configure { id, rect, states } => configure(state, id, rect, states),
        Command::Stack(order) => {
            for id in order {
                if let Some(window) = find_window(state, id)
                    && state.space.elements().any(|w| *w == window)
                {
                    state.space.raise_element(&window, false);
                }
            }
            state.refocus_pointer();
            mark_dirty(state);
        }
        Command::Focus(id) => {
            let target = id.and_then(|id| find_window(state, id));
            if let Some(wm) = state.wm.as_mut() {
                wm.focus = id;
            }
            if let Some(keyboard) = state.seat.get_keyboard() {
                let serial = next_serial();
                keyboard.set_focus(state, target.map(FocusTarget::Window), serial);
            }
        }
        Command::Close(id) => {
            if let Some(window) = find_window(state, id) {
                if let Some(toplevel) = window.toplevel() {
                    toplevel.send_close();
                }
                if let Some(surface) = window.x11_surface() {
                    let _ = surface.close();
                }
            }
        }
        Command::Frame { id, frame } => {
            set_frame(state, id, frame);
            mark_dirty(state);
        }
        Command::Overlay { key, rect, image } => {
            let buffer = buffer_of(&image);
            if let Some(wm) = state.wm.as_mut() {
                wm.overlays.insert(key, Overlay { rect, buffer });
            }
            mark_dirty(state);
        }
        Command::ClearOverlay(key) => {
            if let Some(wm) = state.wm.as_mut()
                && wm.overlays.remove(&key).is_some()
            {
                mark_dirty(state);
            }
        }
        Command::Cursor(name) => {
            let icon = if name == "default" {
                None
            } else {
                name.parse::<CursorIcon>().ok()
            };
            if let Some(wm) = state.wm.as_mut() {
                wm.cursor = icon;
            }
            let image = match icon {
                Some(icon) => CursorImageStatus::Named(icon),
                None => state
                    .current_cursor_icon
                    .clone()
                    .unwrap_or(CursorImageStatus::Named(CursorIcon::Default)),
            };
            state.send_cursor_image(&image);
            mark_dirty(state);
        }
        Command::Effect { id, transform } => {
            if let Some(wm) = state.wm.as_mut() {
                match transform {
                    Some(t) => {
                        wm.effects.insert(id, t);
                    }
                    None => {
                        wm.effects.remove(&id);
                    }
                }
            }
            mark_dirty(state);
        }
        Command::WakeAt(at) => {
            if let Some(wm) = state.wm.as_mut() {
                wm.wake_at = Some(match wm.wake_at {
                    Some(current) => current.min(at),
                    None => at,
                });
            }
        }
        Command::Desktops { count, current } => {
            crate::wayland::workspace::set_desktops(state, count, current);
        }
        Command::Redraw => {
            for node in &mut state.output_nodes {
                if let Some(cap) = node.capture.as_mut() {
                    cap.needs_full_render = true;
                }
            }
            mark_dirty(state);
        }
    }
}

fn set_frame(state: &mut AppState, id: WindowId, frame: Option<Frame>) {
    let Some(wm) = state.wm.as_mut() else {
        return;
    };
    match frame {
        Some(frame) => {
            let parts = frame
                .parts
                .iter()
                .map(|part| (part.rect, buffer_of(&part.image)))
                .collect();
            wm.frames.insert(
                id,
                FrameBuffers {
                    extents: frame.extents,
                    parts,
                },
            );
        }
        None => {
            wm.frames.remove(&id);
        }
    }
    sync_decoration_mode(state, id);
}

/// Tell an xdg client the decoration mode the policy settled on: server-side where it draws
/// a frame, client-side where the client wanted its own and none is drawn.
fn sync_decoration_mode(state: &mut AppState, id: WindowId) {
    use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;
    let Some(window) = find_window(state, id) else {
        return;
    };
    let Some(toplevel) = window.toplevel() else {
        return;
    };
    let framed = state
        .wm
        .as_ref()
        .is_some_and(|wm| wm.frames.contains_key(&id));
    let preference = state
        .wm
        .as_ref()
        .and_then(|wm| wm.decorations.get(&id).copied())
        .unwrap_or_default();
    let mode = if framed || preference != Decoration::Client {
        Mode::ServerSide
    } else {
        Mode::ClientSide
    };
    let current = toplevel.with_committed_state(|c| c.and_then(|c| c.decoration_mode));
    if current != Some(mode) {
        toplevel.with_pending_state(|s| {
            s.decoration_mode = Some(mode);
        });
        toplevel.send_pending_configure();
    }
}

/// Place and size a window as the policy asked: an xdg configure for the size and states, a
/// Space mapping for the position, a Space unmapping for a minimized one or one on another
/// desktop, and the output's enter and leave as the window moves between outputs.
fn configure(state: &mut AppState, id: WindowId, rect: Rect, states: States) {
    if let Some(wm) = state.wm.as_mut() {
        wm.rects.insert(id, rect);
        wm.states.insert(id, states);
    }
    let Some(window) = find_window(state, id) else {
        return;
    };
    let in_space = state.space.elements().any(|w| *w == window);
    let hidden = state
        .wm
        .as_ref()
        .is_some_and(|wm| wm.hidden.contains_key(&id));
    if let Some(toplevel) = window.toplevel() {
        toplevel.with_pending_state(|s| {
            s.size = (rect.w > 0 && rect.h > 0).then(|| (rect.w, rect.h).into());
            set_state(&mut s.states, XdgState::Maximized, states.maximized);
            set_state(&mut s.states, XdgState::Fullscreen, states.fullscreen);
            set_state(&mut s.states, XdgState::Activated, states.activated);
            set_state(&mut s.states, XdgState::Resizing, states.resizing);
            if states.maximized || states.fullscreen {
                set_state(&mut s.states, XdgState::TiledTop, true);
                set_state(&mut s.states, XdgState::TiledBottom, true);
                set_state(&mut s.states, XdgState::TiledLeft, true);
                set_state(&mut s.states, XdgState::TiledRight, true);
            } else {
                set_state(&mut s.states, XdgState::TiledTop, false);
                set_state(&mut s.states, XdgState::TiledBottom, false);
                set_state(&mut s.states, XdgState::TiledLeft, false);
                set_state(&mut s.states, XdgState::TiledRight, false);
            }
        });
        toplevel.send_pending_configure();
    }
    if let Some(surface) = window.x11_surface() {
        crate::wayland::xwm::configure(surface, rect, states);
    }
    if states.minimized || states.hidden {
        if in_space {
            state.space.unmap_elem(&window);
            if let Some(wm) = state.wm.as_mut() {
                wm.hidden.insert(id, window.clone());
            }
            state.refocus_pointer();
        }
    } else if in_space || hidden {
        let location = (rect.x, rect.y);
        let awaits_size = in_space
            && window.toplevel().is_some()
            && state.space.element_location(&window) != Some(location.into())
            && state
                .wm
                .as_ref()
                .and_then(|wm| wm.known.get(&id))
                .is_some_and(|known| {
                    known.w > 0 && known.h > 0 && (known.w != rect.w || known.h != rect.h)
                });
        if let Some(wm) = state.wm.as_mut() {
            wm.hidden.remove(&id);
            if awaits_size {
                wm.pending_moves.insert(id, location);
            } else {
                wm.pending_moves.remove(&id);
            }
        }
        if !awaits_size {
            state.space.map_element(window.clone(), location, false);
            move_to_output_under(state, &window, rect);
            state.refocus_pointer();
        }
    }
    crate::wayland::foreign_toplevel::update(state, id);
    mark_dirty(state);
}

fn set_state(
    states: &mut smithay::wayland::shell::xdg::ToplevelStateSet,
    state: XdgState,
    on: bool,
) {
    if on {
        states.set(state);
    } else {
        states.unset(state);
    }
}

/// Keep the window's output tag and its surface's output enter and leave with the output the
/// client rectangle mostly lies on.
fn move_to_output_under(state: &mut AppState, window: &Window, rect: Rect) {
    let center = Point::<f64, Logical>::from((
        rect.x as f64 + rect.w as f64 / 2.0,
        rect.y as f64 + rect.h as f64 / 2.0,
    ));
    let Some(idx) = state.node_idx_under(center) else {
        return;
    };
    let node_id = state.output_nodes[idx]
        .owner
        .unwrap_or(state.output_nodes[idx].id);
    let Some(idx) = state.node_idx_for_id(node_id) else {
        return;
    };
    let old_id = window_output_id(window);
    if old_id == node_id {
        return;
    }
    let new_output = state.output_nodes[idx].output.clone();
    let old_output = state
        .node_idx_for_id(old_id)
        .map(|i| state.output_nodes[i].output.clone());
    if let Some(meta) = window_meta(window) {
        meta.output
            .store(node_id, std::sync::atomic::Ordering::Relaxed);
    }
    if let Some(surface) = window.wl_surface() {
        if let Some(old) = old_output {
            old.leave(&surface);
        }
        new_output.enter(&surface);
        let scale = new_output.current_scale().fractional_scale();
        with_states(&surface, |states| {
            smithay::wayland::compositor::send_surface_state(
                &surface,
                states,
                scale.ceil() as i32,
                OutputTransform::Normal,
            );
            smithay::wayland::fractional_scale::with_fractional_scale(states, |fs| {
                fs.set_preferred_scale(scale);
            });
        });
    }
}

/// The client rectangle the policy configured a window to.
pub fn rect_of(state: &AppState, id: WindowId) -> Option<Rect> {
    state.wm.as_ref().and_then(|wm| wm.rects.get(&id).copied())
}

/// The states the policy configured a window to.
pub fn states_of(state: &AppState, id: WindowId) -> Option<States> {
    state.wm.as_ref().and_then(|wm| wm.states.get(&id).copied())
}

/// The effect a window is presented through, if one runs.
pub fn effect_of(state: &AppState, id: WindowId) -> Option<Transform> {
    state
        .wm
        .as_ref()
        .and_then(|wm| wm.effects.get(&id).copied())
}

/// Whether a window is hidden from the compositor's hit test: shaded or faded out.
pub fn hit_hidden(state: &AppState, window: &Window) -> bool {
    window_id(window)
        .and_then(|id| effect_of(state, id))
        .is_some_and(|t| t.alpha <= 0.0 || t.rect.h <= 0)
}

/// The render elements of a window's frame around a client area at `client_loc` (layout
/// coordinates), for an output at `origin` and `scale`.
pub fn frame_elements<R>(
    wm: Option<&WmState>,
    renderer: &mut R,
    id: WindowId,
    client_loc: Point<i32, Logical>,
    origin: Point<i32, Logical>,
    scale: f64,
    alpha: f32,
) -> Vec<MemoryRenderBufferRenderElement<R>>
where
    R: Renderer + ImportMem,
    R::TextureId: Texture + Clone + Send + 'static,
{
    let mut elements = Vec::new();
    let Some(frame) = wm.and_then(|wm| wm.frames.get(&id)) else {
        return elements;
    };
    for (rect, buffer) in &frame.parts {
        let loc = Point::<i32, Logical>::from((
            client_loc.x + rect.x - origin.x,
            client_loc.y + rect.y - origin.y,
        ));
        let location: Point<f64, Physical> = loc.to_f64().to_physical(Scale::from(scale));
        if let Ok(element) = MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            location,
            buffer,
            Some(alpha),
            None,
            Some((rect.w, rect.h).into()),
            Kind::Unspecified,
        ) {
            elements.push(element);
        }
    }
    elements
}

/// The render elements of every overlay that touches an output at `origin` of `size`
/// (logical), topmost first.
pub fn overlay_elements<R>(
    wm: Option<&WmState>,
    renderer: &mut R,
    origin: Point<i32, Logical>,
    size: smithay::utils::Size<i32, Logical>,
    scale: f64,
) -> Vec<MemoryRenderBufferRenderElement<R>>
where
    R: Renderer + ImportMem,
    R::TextureId: Texture + Clone + Send + 'static,
{
    let mut elements = Vec::new();
    let Some(wm) = wm else {
        return elements;
    };
    let output = Rectangle::<i32, Logical>::new(origin, size);
    for overlay in wm.overlays.values().rev() {
        let rect = Rectangle::<i32, Logical>::new(
            (overlay.rect.x, overlay.rect.y).into(),
            (overlay.rect.w, overlay.rect.h).into(),
        );
        if !rect.overlaps(output) {
            continue;
        }
        let loc =
            Point::<i32, Logical>::from((overlay.rect.x - origin.x, overlay.rect.y - origin.y));
        let location: Point<f64, Physical> = loc.to_f64().to_physical(Scale::from(scale));
        if let Ok(element) = MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            location,
            &overlay.buffer,
            Some(1.0),
            None,
            Some((overlay.rect.w, overlay.rect.h).into()),
            Kind::Unspecified,
        ) {
            elements.push(element);
        }
    }
    elements
}

/// The frame's extents around a window, zero without one.
pub fn frame_extents(state: &AppState, id: WindowId) -> Extents {
    state
        .wm
        .as_ref()
        .and_then(|wm| wm.frames.get(&id))
        .map(|f| f.extents)
        .unwrap_or_default()
}
