/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! `zwlr_foreign_toplevel_manager_v1`: the window list a taskbar reads and acts on. Every
//! mapped toplevel is a handle on each bound manager, carrying its title, app id, states, and
//! output; a taskbar's requests (activate, minimize, maximize, fullscreen, close) go to the
//! window policy where one is installed, and without one the activate raises and focuses, the
//! close asks the client, and the rest are ignored since every window is fullscreen then. A
//! taskbar's `set_rectangle` tells the policy where it shows the window, the target of a
//! minimize effect, in layout coordinates: the rectangle arrives relative to the taskbar's own
//! surface, a layer surface whose place on its output the layer map knows.

use std::collections::HashMap;

use smithay::desktop::{Window, layer_map_for_output};
use smithay::input::pointer::Focus;
use smithay::reexports::wayland_protocols_wlr::foreign_toplevel::v1::server::{
    zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
    backend::{ClientId, GlobalId},
};
use smithay::utils::Rectangle;

use crate::wayland::frontend::{AppState, FocusTarget, next_serial, window_meta, window_output_id};
use crate::wayland::wm;
use crate::wm::{Rect, Request, WindowId};

/// The user data of a handle: the window it stands for.
#[derive(Clone, Copy, Debug)]
pub struct HandleData {
    pub window: WindowId,
}

/// What every handle of a window was last told.
#[derive(Clone, Debug, Default, PartialEq)]
struct Published {
    title: String,
    app_id: String,
    maximized: bool,
    minimized: bool,
    activated: bool,
    fullscreen: bool,
    output: Option<u32>,
}

/// The manager instances bound and the handles each window has on them.
#[derive(Default)]
pub struct ForeignToplevelState {
    managers: Vec<ZwlrForeignToplevelManagerV1>,
    handles: HashMap<WindowId, Vec<ZwlrForeignToplevelHandleV1>>,
    published: HashMap<WindowId, Published>,
    global: Option<GlobalId>,
}

impl ForeignToplevelState {
    /// Publish the global.
    pub fn new(dh: &DisplayHandle) -> ForeignToplevelState {
        let global = dh.create_global::<AppState, ZwlrForeignToplevelManagerV1, _>(3, ());
        ForeignToplevelState {
            global: Some(global),
            ..ForeignToplevelState::default()
        }
    }

    /// Whether any taskbar is bound.
    pub fn bound(&self) -> bool {
        !self.managers.is_empty()
    }
}

/// Whether a window is listed: a toplevel on screen or hidden, never a parked one.
fn listed(window: &Window) -> bool {
    window_meta(window).is_some_and(|m| !m.parked.load(std::sync::atomic::Ordering::Relaxed))
}

/// What a window's handles say about it now.
fn published_of(state: &AppState, window: &Window, id: WindowId) -> Published {
    let info = wm::window_info(state, window);
    let states = wm::states_of(state, id);
    let focused = match state.wm.as_ref() {
        Some(w) if wm::active(state) => w.focus == Some(id),
        _ => state.keyboard_focus == Some(id),
    };
    let (maximized, minimized, fullscreen) = match states {
        Some(s) => (s.maximized, s.minimized, s.fullscreen),
        None => (false, false, true),
    };
    Published {
        title: info.title,
        app_id: info.app_id,
        maximized,
        minimized,
        activated: focused,
        fullscreen,
        output: Some(window_output_id(window)),
    }
}

fn send_all(
    state: &AppState,
    handle: &ZwlrForeignToplevelHandleV1,
    published: &Published,
    previous: Option<&Published>,
) {
    if previous.is_none_or(|p| p.title != published.title) {
        handle.title(published.title.clone());
    }
    if previous.is_none_or(|p| p.app_id != published.app_id) {
        handle.app_id(published.app_id.clone());
    }
    let states_changed = previous.is_none_or(|p| {
        (p.maximized, p.minimized, p.activated, p.fullscreen)
            != (
                published.maximized,
                published.minimized,
                published.activated,
                published.fullscreen,
            )
    });
    if states_changed {
        let mut states: Vec<u8> = Vec::new();
        let mut push = |s: zwlr_foreign_toplevel_handle_v1::State| {
            states.extend_from_slice(&(s as u32).to_ne_bytes());
        };
        if published.maximized {
            push(zwlr_foreign_toplevel_handle_v1::State::Maximized);
        }
        if published.minimized {
            push(zwlr_foreign_toplevel_handle_v1::State::Minimized);
        }
        if published.activated {
            push(zwlr_foreign_toplevel_handle_v1::State::Activated);
        }
        if published.fullscreen && handle.version() >= 2 {
            push(zwlr_foreign_toplevel_handle_v1::State::Fullscreen);
        }
        handle.state(states);
    }
    if previous.is_none_or(|p| p.output != published.output) {
        if let Some(old) = previous.and_then(|p| p.output)
            && let Some(idx) = state.node_idx_for_id(old)
            && let Some(client) = handle.client()
        {
            for wl_output in state.output_nodes[idx].output.client_outputs(&client) {
                handle.output_leave(&wl_output);
            }
        }
        if let Some(new) = published.output
            && let Some(idx) = state.node_idx_for_id(new)
            && let Some(client) = handle.client()
        {
            for wl_output in state.output_nodes[idx].output.client_outputs(&client) {
                handle.output_enter(&wl_output);
            }
        }
    }
    handle.done();
}

/// A window mapped: give every bound taskbar a handle for it.
pub fn add(state: &mut AppState, window: &Window) {
    let Some(id) = window_meta(window).map(|m| m.id) else {
        return;
    };
    if !listed(window) || state.foreign_toplevel.handles.contains_key(&id) {
        return;
    }
    let published = published_of(state, window, id);
    let managers = state.foreign_toplevel.managers.clone();
    let mut handles = Vec::new();
    for manager in &managers {
        let Some(client) = manager.client() else {
            continue;
        };
        let Ok(handle) = client
            .create_resource::<ZwlrForeignToplevelHandleV1, HandleData, AppState>(
                &state.dh,
                manager.version(),
                HandleData { window: id },
            )
        else {
            continue;
        };
        manager.toplevel(&handle);
        send_all(state, &handle, &published, None);
        handles.push(handle);
    }
    state.foreign_toplevel.handles.insert(id, handles);
    state.foreign_toplevel.published.insert(id, published);
}

/// A window's title, app id, states, or output may have changed: tell its handles.
pub fn update(state: &mut AppState, id: WindowId) {
    let Some(window) = wm::find_window(state, id) else {
        return;
    };
    if !state.foreign_toplevel.handles.contains_key(&id) {
        add(state, &window);
        return;
    }
    let published = published_of(state, &window, id);
    let previous = state.foreign_toplevel.published.get(&id).cloned();
    if previous.as_ref() == Some(&published) {
        return;
    }
    let handles = state
        .foreign_toplevel
        .handles
        .get(&id)
        .cloned()
        .unwrap_or_default();
    for handle in &handles {
        send_all(state, handle, &published, previous.as_ref());
    }
    state.foreign_toplevel.published.insert(id, published);
}

/// Every listed window's handles brought up to date, after a focus change.
pub fn update_all(state: &mut AppState) {
    let ids: Vec<WindowId> = state.foreign_toplevel.handles.keys().copied().collect();
    for id in ids {
        update(state, id);
    }
}

/// A window is gone, or withdrawn: close its handles.
pub fn remove(state: &mut AppState, id: WindowId) {
    if let Some(handles) = state.foreign_toplevel.handles.remove(&id) {
        for handle in handles {
            handle.closed();
        }
    }
    state.foreign_toplevel.published.remove(&id);
}

impl GlobalDispatch<ZwlrForeignToplevelManagerV1, ()> for AppState {
    fn bind(
        state: &mut Self,
        _dh: &DisplayHandle,
        client: &Client,
        resource: New<ZwlrForeignToplevelManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let manager = data_init.init(resource, ());
        let ids: Vec<WindowId> = state.foreign_toplevel.handles.keys().copied().collect();
        for id in ids {
            let Some(published) = state.foreign_toplevel.published.get(&id).cloned() else {
                continue;
            };
            let Ok(handle) = client
                .create_resource::<ZwlrForeignToplevelHandleV1, HandleData, AppState>(
                    &state.dh,
                    manager.version(),
                    HandleData { window: id },
                )
            else {
                continue;
            };
            manager.toplevel(&handle);
            send_all(state, &handle, &published, None);
            if let Some(handles) = state.foreign_toplevel.handles.get_mut(&id) {
                handles.push(handle);
            }
        }
        state.foreign_toplevel.managers.push(manager);
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for AppState {
    fn request(
        state: &mut Self,
        _client: &Client,
        manager: &ZwlrForeignToplevelManagerV1,
        request: zwlr_foreign_toplevel_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        if let zwlr_foreign_toplevel_manager_v1::Request::Stop = request {
            manager.finished();
            state.foreign_toplevel.managers.retain(|m| m != manager);
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        manager: &ZwlrForeignToplevelManagerV1,
        _data: &(),
    ) {
        state.foreign_toplevel.managers.retain(|m| m != manager);
        for handles in state.foreign_toplevel.handles.values_mut() {
            handles.retain(|h| {
                h.client().map(|c| c.id()) != manager.client().map(|c| c.id()) || h.is_alive()
            });
        }
    }
}

impl Dispatch<ZwlrForeignToplevelHandleV1, HandleData> for AppState {
    fn request(
        state: &mut Self,
        _client: &Client,
        handle: &ZwlrForeignToplevelHandleV1,
        request: zwlr_foreign_toplevel_handle_v1::Request,
        data: &HandleData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        use zwlr_foreign_toplevel_handle_v1::Request as R;
        let id = data.window;
        let policy =
            |state: &mut AppState, request: Request| -> bool { wm::request(state, id, request) };
        match request {
            R::SetMaximized => {
                policy(state, Request::Maximize);
            }
            R::UnsetMaximized => {
                policy(state, Request::Unmaximize);
            }
            R::SetMinimized => {
                policy(state, Request::Minimize);
            }
            R::UnsetMinimized => {
                policy(state, Request::Unminimize);
            }
            R::SetFullscreen { output } => {
                let id_of = output.and_then(|o| {
                    state
                        .output_nodes
                        .iter()
                        .find(|n| n.output.owns(&o))
                        .map(|n| n.id)
                });
                policy(state, Request::Fullscreen(id_of));
            }
            R::UnsetFullscreen => {
                policy(state, Request::Unfullscreen);
            }
            R::Activate { seat: _ } => {
                if !policy(state, Request::Activate) {
                    activate_plain(state, id);
                }
            }
            R::Close => {
                if !policy(state, Request::Close)
                    && let Some(window) = wm::find_window(state, id)
                {
                    if let Some(toplevel) = window.toplevel() {
                        toplevel.send_close();
                    }
                    if let Some(surface) = window.x11_surface() {
                        let _ = surface.close();
                    }
                }
            }
            R::SetRectangle {
                surface,
                x,
                y,
                width,
                height,
            } => {
                let rect = (width > 0 && height > 0)
                    .then(|| layer_surface_origin(state, &surface))
                    .flatten()
                    .map(|(ox, oy)| Rect::new(ox + x, oy + y, width, height));
                wm::taskbar_rect(state, id, rect);
            }
            R::Destroy => {
                if let Some(handles) = state.foreign_toplevel.handles.get_mut(&id) {
                    handles.retain(|h| h != handle);
                }
            }
            _ => {}
        }
    }
}

/// Where a taskbar's surface sits on the layout: a layer surface's geometry on its output.
fn layer_surface_origin(state: &AppState, surface: &WlSurface) -> Option<(i32, i32)> {
    for node in &state.output_nodes {
        let map = layer_map_for_output(&node.output);
        for layer in map.layers() {
            if layer.wl_surface() == surface {
                let geo: Rectangle<i32, smithay::utils::Logical> = map.layer_geometry(layer)?;
                return Some((node.pos.0 + geo.loc.x, node.pos.1 + geo.loc.y));
            }
        }
    }
    None
}

/// Raise and focus a window where no policy decides.
fn activate_plain(state: &mut AppState, id: WindowId) {
    let Some(window) = wm::find_window(state, id) else {
        return;
    };
    state.space.raise_element(&window, true);
    if let Some(keyboard) = state.seat.get_keyboard() {
        keyboard.set_focus(state, Some(FocusTarget::Window(window)), next_serial());
    }
    let _ = Focus::Keep;
    update_all(state);
}
