/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! `ext_workspace_manager_v1`: the virtual desktops a pager reads and switches.
//!
//! The standard protocol is served rather than an interface of this tree's own so that the
//! desktop shell and any other pager (a panel, a dock) read the desktops the way they read
//! them from every other compositor. The compositor keeps only what the protocol shows: how
//! many desktops there are and which is current, as the window policy last said
//! ([`Command::Desktops`]), one desktop and the first before it says anything. Which desktop a
//! window is on is the policy's alone: it hides a window on another desktop through
//! [`States::hidden`], and a taskbar learns of it through the window list, where the hidden
//! window leaves every output while it stays listed.
//!
//! Every bound manager sees one workspace group spanning every output, since a desktop here
//! covers the whole layout, and in it a workspace per desktop, named from one and placed by a
//! single coordinate, the desktop's index. A pager's `activate` is recorded against its manager
//! and acted on at the `commit` that ends the pager's atomic batch, as
//! [`Event::ActivateDesktop`] to the policy; nothing is sent back then, since the policy's
//! answer, a [`Command::Desktops`], is what moves the state every pager sees, so a switch the
//! policy declines leaves the pager showing what the compositor shows.
//!
//! [`Command::Desktops`]: crate::wm::Command::Desktops
//! [`Event::ActivateDesktop`]: crate::wm::Event::ActivateDesktop
//! [`States::hidden`]: crate::wm::States::hidden

use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
    backend::{ClientId, GlobalId},
};
use wayland_protocols::ext::workspace::v1::server::{
    ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1, GroupCapabilities},
    ext_workspace_handle_v1::{self, ExtWorkspaceHandleV1, State, WorkspaceCapabilities},
    ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
};

use crate::wayland::frontend::AppState;
use crate::wayland::wm;

/// The user data of a workspace handle: the manager it was sent on and the desktop it stands
/// for, counted from zero.
#[derive(Clone, Debug)]
pub struct HandleData {
    manager: ExtWorkspaceManagerV1,
    index: u32,
}

/// One bound manager: its group, a handle per desktop in desktop order, and the switch a pager
/// asked for ahead of its commit.
struct Manager {
    manager: ExtWorkspaceManagerV1,
    group: ExtWorkspaceGroupHandleV1,
    handles: Vec<ExtWorkspaceHandleV1>,
    pending: Option<u32>,
}

/// The desktops and the managers bound to them.
pub struct WorkspaceState {
    managers: Vec<Manager>,
    count: u32,
    current: u32,
    /// The outputs every group spans, by display id, kept so a removed output is still left
    /// after its node is gone.
    outputs: Vec<(u32, Output)>,
    global: Option<GlobalId>,
}

impl WorkspaceState {
    /// Publish the global, with one desktop until the policy says otherwise.
    pub fn new(dh: &DisplayHandle) -> WorkspaceState {
        let global = dh.create_global::<AppState, ExtWorkspaceManagerV1, _>(1, ());
        WorkspaceState {
            managers: Vec::new(),
            count: 1,
            current: 0,
            outputs: Vec::new(),
            global: Some(global),
        }
    }
}

/// The desktops a command describes, bounded: at least one, and the current among them.
fn bounded(count: u32, current: u32) -> (u32, u32) {
    let count = count.max(1);
    (count, current.min(count - 1))
}

/// A desktop's coordinates: one dimension, its index, as the array of words the protocol
/// carries in the byte order the client reads them back in.
fn coordinates(index: u32) -> Vec<u8> {
    index.to_ne_bytes().to_vec()
}

/// A desktop's state bits: active for the current one, none for the rest.
fn state_bits(index: u32, current: u32) -> State {
    if index == current {
        State::Active
    } else {
        State::empty()
    }
}

/// The desktops whose active bit moves when the current one changes.
fn active_changes(count: u32, from: u32, to: u32) -> Vec<(u32, State)> {
    (0..count)
        .filter(|index| state_bits(*index, from) != state_bits(*index, to))
        .map(|index| (index, state_bits(index, to)))
        .collect()
}

/// The outputs a group spans: every screen of its own, never a view onto another's.
fn spanned_outputs(state: &AppState) -> Vec<(u32, Output)> {
    state
        .output_nodes
        .iter()
        .filter(|n| n.owner.is_none() && n.global.is_some())
        .map(|n| (n.id, n.output.clone()))
        .collect()
}

/// Send a pager a handle for desktop `index`, assigned to its group, or `None` where its
/// connection is gone.
fn send_workspace(
    state: &AppState,
    entry: &Manager,
    index: u32,
    current: u32,
    client: &Client,
) -> Option<ExtWorkspaceHandleV1> {
    let handle = client
        .create_resource::<ExtWorkspaceHandleV1, HandleData, AppState>(
            &state.dh,
            entry.manager.version(),
            HandleData {
                manager: entry.manager.clone(),
                index,
            },
        )
        .ok()?;
    entry.manager.workspace(&handle);
    let name = (index + 1).to_string();
    handle.id(name.clone());
    handle.name(name);
    handle.coordinates(coordinates(index));
    handle.capabilities(WorkspaceCapabilities::Activate);
    handle.state(state_bits(index, current));
    entry.group.workspace_enter(&handle);
    Some(handle)
}

/// The policy said how many desktops there are and which is current: every pager is told
/// what changed, in one batch.
pub fn set_desktops(state: &mut AppState, count: u32, current: u32) {
    let (count, current) = bounded(count, current);
    let previous = state.workspace.current;
    if (count, current) == (state.workspace.count, previous) {
        return;
    }
    state.workspace.count = count;
    state.workspace.current = current;
    let mut managers = std::mem::take(&mut state.workspace.managers);
    for entry in &mut managers {
        let Some(client) = entry.manager.client() else {
            continue;
        };
        while entry.handles.len() as u32 > count {
            if let Some(handle) = entry.handles.pop() {
                entry.group.workspace_leave(&handle);
                handle.removed();
            }
        }
        let kept = entry.handles.len() as u32;
        for (index, bits) in active_changes(kept, previous, current) {
            entry.handles[index as usize].state(bits);
        }
        while (entry.handles.len() as u32) < count {
            let index = entry.handles.len() as u32;
            match send_workspace(state, entry, index, current, &client) {
                Some(handle) => entry.handles.push(handle),
                None => break,
            }
        }
        entry.manager.done();
    }
    state.workspace.managers = managers;
}

/// The layout's outputs changed: every group enters the new ones and leaves the gone ones.
pub fn outputs_changed(state: &mut AppState) {
    let outputs = spanned_outputs(state);
    let same = outputs.len() == state.workspace.outputs.len()
        && outputs
            .iter()
            .zip(&state.workspace.outputs)
            .all(|((a, _), (b, _))| a == b);
    if same {
        return;
    }
    let previous = std::mem::replace(&mut state.workspace.outputs, outputs);
    for entry in &state.workspace.managers {
        let Some(client) = entry.manager.client() else {
            continue;
        };
        for (id, output) in &previous {
            if !state.workspace.outputs.iter().any(|(i, _)| i == id) {
                for wl_output in output.client_outputs(&client) {
                    entry.group.output_leave(&wl_output);
                }
            }
        }
        for (id, output) in &state.workspace.outputs {
            if !previous.iter().any(|(i, _)| i == id) {
                for wl_output in output.client_outputs(&client) {
                    entry.group.output_enter(&wl_output);
                }
            }
        }
        entry.manager.done();
    }
}

/// A client bound a `wl_output` of an output its group already spans: the group enters it,
/// since an enter reaches only the objects the client held when it was sent.
pub fn output_bound(state: &mut AppState, output: &Output, wl_output: &WlOutput) {
    if !state.workspace.outputs.iter().any(|(_, o)| o == output) {
        return;
    }
    let client = wl_output.client().map(|c| c.id());
    for entry in &state.workspace.managers {
        if entry.manager.client().map(|c| c.id()) != client {
            continue;
        }
        entry.group.output_enter(wl_output);
        entry.manager.done();
    }
}

impl GlobalDispatch<ExtWorkspaceManagerV1, ()> for AppState {
    fn bind(
        state: &mut Self,
        _dh: &DisplayHandle,
        client: &Client,
        resource: New<ExtWorkspaceManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let manager = data_init.init(resource, ());
        outputs_changed(state);
        let Ok(group) = client.create_resource::<ExtWorkspaceGroupHandleV1, (), AppState>(
            &state.dh,
            manager.version(),
            (),
        ) else {
            return;
        };
        manager.workspace_group(&group);
        group.capabilities(GroupCapabilities::empty());
        for (_, output) in &state.workspace.outputs {
            for wl_output in output.client_outputs(client) {
                group.output_enter(&wl_output);
            }
        }
        let mut entry = Manager {
            manager,
            group,
            handles: Vec::new(),
            pending: None,
        };
        for index in 0..state.workspace.count {
            match send_workspace(state, &entry, index, state.workspace.current, client) {
                Some(handle) => entry.handles.push(handle),
                None => break,
            }
        }
        entry.manager.done();
        state.workspace.managers.push(entry);
    }
}

impl Dispatch<ExtWorkspaceManagerV1, ()> for AppState {
    fn request(
        state: &mut Self,
        _client: &Client,
        manager: &ExtWorkspaceManagerV1,
        request: ext_workspace_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            ext_workspace_manager_v1::Request::Commit => {
                let pending = state
                    .workspace
                    .managers
                    .iter_mut()
                    .find(|m| m.manager == *manager)
                    .and_then(|m| m.pending.take());
                if let Some(index) = pending
                    && index < state.workspace.count
                {
                    wm::activate_desktop(state, index);
                }
            }
            ext_workspace_manager_v1::Request::Stop => {
                manager.finished();
                state.workspace.managers.retain(|m| m.manager != *manager);
            }
            _ => {}
        }
    }

    fn destroyed(state: &mut Self, _client: ClientId, manager: &ExtWorkspaceManagerV1, _data: &()) {
        state.workspace.managers.retain(|m| m.manager != *manager);
    }
}

impl Dispatch<ExtWorkspaceGroupHandleV1, ()> for AppState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _group: &ExtWorkspaceGroupHandleV1,
        _request: ext_workspace_group_handle_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

impl Dispatch<ExtWorkspaceHandleV1, HandleData> for AppState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _handle: &ExtWorkspaceHandleV1,
        request: ext_workspace_handle_v1::Request,
        data: &HandleData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        if let ext_workspace_handle_v1::Request::Activate = request
            && let Some(entry) = state
                .workspace
                .managers
                .iter_mut()
                .find(|m| m.manager == data.manager)
        {
            entry.pending = Some(data.index);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A desktop's coordinates are its index alone, one word in the client's byte order.
    #[test]
    fn coordinates_are_the_index_as_one_word() {
        assert_eq!(coordinates(0), 0u32.to_ne_bytes().to_vec());
        assert_eq!(coordinates(3), 3u32.to_ne_bytes().to_vec());
        assert_eq!(coordinates(3).len(), 4);
    }

    /// The current desktop alone carries the active bit, and a switch moves it from one
    /// desktop to the other and touches no third.
    #[test]
    fn the_active_bit_moves_with_the_current_desktop() {
        assert_eq!(state_bits(2, 2), State::Active);
        assert_eq!(state_bits(1, 2), State::empty());
        assert_eq!(
            active_changes(4, 0, 2),
            vec![(0, State::empty()), (2, State::Active)]
        );
        assert!(active_changes(4, 1, 1).is_empty());
        assert_eq!(active_changes(2, 0, 3), vec![(0, State::empty())]);
    }

    /// A command naming no desktop, or a current one past the count, is bounded to what the
    /// protocol can show.
    #[test]
    fn desktops_are_bounded_to_at_least_one_with_the_current_among_them() {
        assert_eq!(bounded(0, 0), (1, 0));
        assert_eq!(bounded(4, 9), (4, 3));
        assert_eq!(bounded(4, 2), (4, 2));
    }
}
