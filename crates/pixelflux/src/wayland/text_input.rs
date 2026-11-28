/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! `zwp_text_input_v3`: where the focused text field's caret is. An application that speaks the
//! protocol (GTK 3 and 4, Qt 6, Firefox, Chromium with its Wayland IME switched on) enables its
//! text input while one of its fields has the focus and reports the field's cursor rectangle,
//! which a remote client needs to open its own input method's candidates beside the remote
//! caret.
//!
//! The compositor is no input method. It never sends `preedit_string`, `commit_string`,
//! `delete_surrounding_text`, or `done`, and takes no keyboard grab, so keys keep arriving as
//! `wl_keyboard` events and an application types exactly as it does without the protocol. It
//! sends `enter` and `leave` with the keyboard focus, because an application enables its text
//! input, and so reports a rectangle, only on a surface it was told it entered. smithay's
//! `TextInputManagerState` does not serve here: it discards every request and sends no `enter`
//! while no input method is bound, and it keeps the rectangle where the compositor cannot read
//! it.
//!
//! The rectangle arrives relative to the surface that has keyboard focus. Once per event-loop
//! pass ([`publish`]) it is placed at that surface's position in the layout and mapped into
//! physical union-layout pixels, the space pointer motion is injected in, so a window moved or
//! rescaled under a still caret moves it too; a change goes to the consumer registered with
//! [`set_sink`] on the `wl-caret` thread, since no sink is called on the compositor thread.

use std::borrow::Cow;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};

use smithay::desktop::{PopupManager, find_popup_root_surface, layer_map_for_output};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, backend::ClientId,
};
use smithay::utils::{Logical, Point, Rectangle};
use smithay::wayland::seat::WaylandFocus;
use wayland_protocols::wp::text_input::zv3::server::{
    zwp_text_input_manager_v3::{self, ZwpTextInputManagerV3},
    zwp_text_input_v3::{self, ZwpTextInputV3},
};

use crate::sink::{TextCaret, TextCaretSink};
use crate::wayland::frontend::{AppState, FocusTarget};

/// The text inputs bound and the focus they follow.
#[derive(Default)]
pub struct TextInputState {
    inputs: Vec<ZwpTextInputV3>,
    /// The window, popup, or layer surface the keyboard focus is on, whose client's text inputs
    /// were sent `enter`.
    focus: Option<FocusTarget>,
    /// The caret last handed to the `wl-caret` thread.
    published: Option<TextCaret>,
}

impl TextInputState {
    /// Publish the global.
    pub fn new(dh: &DisplayHandle) -> TextInputState {
        dh.create_global::<AppState, ZwpTextInputManagerV3, _>(1, ());
        TextInputState::default()
    }
}

/// The user data of a text input: its double-buffered state, applied on `commit`.
#[derive(Default)]
pub struct TextInputData {
    inner: Mutex<Buffered>,
}

/// What a text input's client asked for, pending and applied.
#[derive(Default)]
struct Buffered {
    /// Between an `enter` and the `leave` that ends it; requests outside one are ignored, as the
    /// protocol says.
    entered: bool,
    pending_enabled: Option<bool>,
    pending_rect: Option<Rectangle<i32, Logical>>,
    enabled: bool,
    /// The cursor rectangle, relative to the entered surface.
    rect: Option<Rectangle<i32, Logical>>,
}

impl TextInputData {
    fn buffered(&self) -> std::sync::MutexGuard<'_, Buffered> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl GlobalDispatch<ZwpTextInputManagerV3, ()> for AppState {
    fn bind(
        _state: &mut Self,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwpTextInputManagerV3>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwpTextInputManagerV3, ()> for AppState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ZwpTextInputManagerV3,
        request: zwp_text_input_manager_v3::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        let zwp_text_input_manager_v3::Request::GetTextInput { id, seat: _ } = request else {
            return;
        };
        let input = data_init.init(id, TextInputData::default());
        // A field that already has the keyboard's focus is entered at once: the client
        // enables its text input on the `enter` alone.
        if let Some(surface) = focused_surface(state)
            && same_client(&input, &surface)
        {
            enter(&input, &surface);
        }
        state.text_input.inputs.push(input);
    }
}

impl Dispatch<ZwpTextInputV3, TextInputData> for AppState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ZwpTextInputV3,
        request: zwp_text_input_v3::Request,
        data: &TextInputData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let mut b = data.buffered();
        if !b.entered {
            return;
        }
        match request {
            zwp_text_input_v3::Request::Enable => {
                // Enabling resets what earlier requests set.
                b.pending_enabled = Some(true);
                b.pending_rect = None;
            }
            zwp_text_input_v3::Request::Disable => b.pending_enabled = Some(false),
            zwp_text_input_v3::Request::SetCursorRectangle {
                x,
                y,
                width,
                height,
            } => b.pending_rect = Some(Rectangle::new((x, y).into(), (width, height).into())),
            zwp_text_input_v3::Request::Commit => {
                if let Some(enabled) = b.pending_enabled.take() {
                    b.enabled = enabled;
                    b.rect = None;
                }
                if let Some(rect) = b.pending_rect.take() {
                    b.rect = Some(rect);
                }
            }
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        input: &ZwpTextInputV3,
        _data: &TextInputData,
    ) {
        state.text_input.inputs.retain(|i| i != input);
    }
}

/// The keyboard focus moved: the text inputs of the client it left are sent `leave`, and those
/// of the client it entered `enter`, on the window, popup, or layer surface that has it.
pub fn focus_changed(state: &mut AppState, focus: Option<&FocusTarget>) {
    let focus = focus.map(|f| f.owner().clone());
    let left = focused_surface(state);
    let entered = focus
        .as_ref()
        .and_then(|f| f.wl_surface().map(Cow::into_owned));
    state.text_input.focus = focus;
    if left == entered {
        return;
    }
    for input in &state.text_input.inputs {
        if let Some(surface) = left.as_ref().filter(|s| same_client(input, s)) {
            leave(input, surface);
        }
        if let Some(surface) = entered.as_ref().filter(|s| same_client(input, s)) {
            enter(input, surface);
        }
    }
}

/// Hand the caret to the `wl-caret` thread where it changed since the last pass.
pub fn publish(state: &mut AppState) {
    let caret = caret(state);
    if caret != state.text_input.published {
        state.text_input.published = caret;
        deliver(Job::Caret(caret));
    }
}

/// Register the consumer of the caret, `None` to withdraw it. A new one is told the caret at
/// once.
pub fn set_sink(sink: Option<Arc<dyn TextCaretSink>>) {
    deliver(Job::Sink(sink));
}

fn focused_surface(state: &AppState) -> Option<WlSurface> {
    state
        .text_input
        .focus
        .as_ref()
        .and_then(|f| f.wl_surface().map(Cow::into_owned))
}

fn same_client(input: &ZwpTextInputV3, surface: &WlSurface) -> bool {
    input.id().same_client_as(&surface.id())
}

/// Send `enter`, after which the client enables its text input afresh.
fn enter(input: &ZwpTextInputV3, surface: &WlSurface) {
    if let Some(data) = input.data::<TextInputData>() {
        *data.buffered() = Buffered {
            entered: true,
            ..Buffered::default()
        };
    }
    input.enter(surface);
}

/// End what the client enabled, and send `leave` where the surface is still there to name.
fn leave(input: &ZwpTextInputV3, surface: &WlSurface) {
    if let Some(data) = input.data::<TextInputData>() {
        *data.buffered() = Buffered::default();
    }
    if surface.is_alive() {
        input.leave(surface);
    }
}

/// The enabled text input's cursor rectangle in physical union-layout pixels; None while no
/// field of the focused surface's client has its text input enabled with a rectangle, or the
/// surface is nowhere in the layout.
fn caret(state: &AppState) -> Option<TextCaret> {
    let focus = state.text_input.focus.as_ref()?;
    let surface = focus.wl_surface()?;
    let rect = state
        .text_input
        .inputs
        .iter()
        .filter(|input| same_client(input, &surface))
        .find_map(|input| {
            let b = input.data::<TextInputData>()?.buffered();
            b.enabled.then_some(b.rect).flatten()
        })?;
    let at = surface_origin(state, focus)? + rect.loc.to_f64();
    let (x, y, scale) = state.layout_logical_to_physical_scaled(at);
    Some(TextCaret {
        x: x.round() as i32,
        y: y.round() as i32,
        width: (f64::from(rect.size.w) * scale).round() as i32,
        height: (f64::from(rect.size.h) * scale).round() as i32,
    })
}

/// Where a focus target's own surface lies in the layout, in logical pixels, found as
/// `FocusTarget` hit-testing in the frontend places windows, popups, and layer surfaces for the
/// pointer.
fn surface_origin(state: &AppState, focus: &FocusTarget) -> Option<Point<f64, Logical>> {
    match focus {
        FocusTarget::Window(window) => state
            .space
            .element_location(window)
            .map(|loc| (loc - window.geometry().loc).to_f64()),
        FocusTarget::Popup(popup) => {
            let root = find_popup_root_surface(popup).ok()?;
            let window = state
                .space
                .elements()
                .find(|w| w.wl_surface().as_deref() == Some(&root))?;
            let loc = state.space.element_location(window)?;
            let (_, offset) = PopupManager::popups_for_surface(&root).find(|(p, _)| p == popup)?;
            Some((loc + offset - popup.geometry().loc).to_f64())
        }
        FocusTarget::LayerSurface(layer) => state.output_nodes.iter().find_map(|node| {
            let map = layer_map_for_output(&node.output);
            let bbox = map.layer_geometry(layer)?;
            Some((bbox.loc + Point::<i32, Logical>::from(node.pos)).to_f64())
        }),
        FocusTarget::Subsurface(_, owner) => surface_origin(state, owner),
    }
}

/// One unit of work for the `wl-caret` thread; one channel keeps the carets ordered with the
/// sink's registration.
enum Job {
    Sink(Option<Arc<dyn TextCaretSink>>),
    Caret(Option<TextCaret>),
}

/// Queue a job, starting the `wl-caret` thread on the first one.
fn deliver(job: Job) {
    static TX: OnceLock<Option<Sender<Job>>> = OnceLock::new();
    let tx = TX.get_or_init(|| {
        let (tx, rx) = channel::<Job>();
        std::thread::Builder::new()
            .name("wl-caret".into())
            .spawn(move || {
                let mut sink: Option<Arc<dyn TextCaretSink>> = None;
                let mut last = None;
                for job in rx {
                    match job {
                        Job::Sink(new) => {
                            sink = new;
                            if let Some(sink) = &sink {
                                sink.caret(last);
                            }
                        }
                        Job::Caret(caret) => {
                            last = caret;
                            if let Some(sink) = &sink {
                                sink.caret(caret);
                            }
                        }
                    }
                }
            })
            .ok()
            .map(|_| tx)
    });
    if let Some(tx) = tx {
        let _ = tx.send(job);
    }
}
