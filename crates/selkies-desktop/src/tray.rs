//! The system tray: a host for XEmbed icons, the protocol the applications
//! that park an icon beside the clock still speak.  The host owns the
//! `_NET_SYSTEM_TRAY_S<n>` selection on the X display of the session, which
//! under labwc is XWayland, and reparents each docked icon window into a
//! small override-redirect window placed over the bar's tray strip.  The X
//! server paints and delivers input to the icons itself: under openbox the
//! strip sits above the dock window, under labwc XWayland shows it above
//! the panel layer, so no pixels are copied and the icons' own menus open
//! where they expect.

use std::os::fd::{AsFd, AsRawFd, RawFd};

use x11rb::connection::Connection as _;
use x11rb::protocol::Event as XEvent;
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::geom::Rect;

/// Opcode of a dock request in a tray client message.
const SYSTEM_TRAY_REQUEST_DOCK: u32 = 0;
/// The XEmbed message telling an icon it was embedded.
const XEMBED_EMBEDDED_NOTIFY: u32 = 0;
/// Space between icons, in physical pixels at scale one.
const GAP: i32 = 4;

x11rb::atom_manager! {
    pub TrayAtoms: TrayAtomsCookie {
        MANAGER,
        _NET_SYSTEM_TRAY_OPCODE,
        _NET_SYSTEM_TRAY_ORIENTATION,
        _NET_SYSTEM_TRAY_VISUAL,
        _XEMBED,
        _XEMBED_INFO,
    }
}

/// The tray host.
pub struct Tray {
    conn: RustConnection,
    root: xproto::Window,
    manager: xproto::Window,
    holder: xproto::Window,
    atoms: TrayAtoms,
    selection: xproto::Atom,
    icons: Vec<xproto::Window>,
    /// Where the strip is, in physical root coordinates, when shown.
    slot: Option<Rect>,
    /// Side of an icon in physical pixels.
    icon_size: i32,
    mapped: bool,
    /// Whether icons came or went since the strip was last arranged.
    changed: bool,
}

impl Tray {
    /// Connects to the X display and takes the tray selection.
    ///
    /// # Returns
    /// The host, or `None` when there is no X display, the selection is
    /// already owned, or the setup failed.
    pub fn new() -> Option<Tray> {
        let (conn, screen_num) = RustConnection::connect(None).ok()?;
        let setup = conn.setup().clone();
        let screen = &setup.roots[screen_num];
        let root = screen.root;
        let atoms = TrayAtoms::new(&conn).ok()?.reply().ok()?;
        let selection = conn
            .intern_atom(false, format!("_NET_SYSTEM_TRAY_S{screen_num}").as_bytes())
            .ok()?
            .reply()
            .ok()?
            .atom;
        if conn.get_selection_owner(selection).ok()?.reply().ok()?.owner != x11rb::NONE {
            return None;
        }
        let argb = screen
            .allowed_depths
            .iter()
            .find(|d| d.depth == 32)
            .and_then(|d| d.visuals.first())
            .map(|v| v.visual_id);
        let manager = conn.generate_id().ok()?;
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            manager,
            root,
            -1,
            -1,
            1,
            1,
            0,
            xproto::WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &xproto::CreateWindowAux::new().override_redirect(1),
        )
        .ok()?;
        let holder = conn.generate_id().ok()?;
        match argb {
            Some(visual) => {
                let colormap = conn.generate_id().ok()?;
                conn.create_colormap(xproto::ColormapAlloc::NONE, colormap, root, visual)
                    .ok()?;
                conn.create_window(
                    32,
                    holder,
                    root,
                    0,
                    0,
                    1,
                    1,
                    0,
                    xproto::WindowClass::INPUT_OUTPUT,
                    visual,
                    &xproto::CreateWindowAux::new()
                        .override_redirect(1)
                        .background_pixel(0)
                        .border_pixel(0)
                        .colormap(colormap)
                        .event_mask(
                            xproto::EventMask::SUBSTRUCTURE_NOTIFY | xproto::EventMask::STRUCTURE_NOTIFY,
                        ),
                )
                .ok()?;
            }
            None => {
                conn.create_window(
                    x11rb::COPY_DEPTH_FROM_PARENT,
                    holder,
                    root,
                    0,
                    0,
                    1,
                    1,
                    0,
                    xproto::WindowClass::INPUT_OUTPUT,
                    screen.root_visual,
                    &xproto::CreateWindowAux::new()
                        .override_redirect(1)
                        .background_pixel(screen.black_pixel)
                        .event_mask(
                            xproto::EventMask::SUBSTRUCTURE_NOTIFY | xproto::EventMask::STRUCTURE_NOTIFY,
                        ),
                )
                .ok()?;
            }
        }
        let _ = conn.change_property32(
            xproto::PropMode::REPLACE,
            manager,
            atoms._NET_SYSTEM_TRAY_ORIENTATION,
            xproto::AtomEnum::CARDINAL,
            &[0],
        );
        let _ = conn.change_property32(
            xproto::PropMode::REPLACE,
            manager,
            atoms._NET_SYSTEM_TRAY_VISUAL,
            xproto::AtomEnum::VISUALID,
            &[argb.unwrap_or(screen.root_visual)],
        );
        conn.set_selection_owner(manager, selection, x11rb::CURRENT_TIME)
            .ok()?;
        if conn.get_selection_owner(selection).ok()?.reply().ok()?.owner != manager {
            return None;
        }
        let announce = xproto::ClientMessageEvent::new(
            32,
            root,
            atoms.MANAGER,
            [x11rb::CURRENT_TIME, selection, manager, 0, 0],
        );
        let _ = conn.send_event(false, root, xproto::EventMask::STRUCTURE_NOTIFY, announce);
        conn.flush().ok()?;
        Some(Tray {
            conn,
            root,
            manager,
            holder,
            atoms,
            selection,
            icons: Vec::new(),
            slot: None,
            icon_size: 22,
            mapped: false,
            changed: false,
        })
    }

    /// The descriptor to poll for readability.
    pub fn fd(&self) -> RawFd {
        self.conn.stream().as_fd().as_raw_fd()
    }

    /// Icons docked at the moment.
    pub fn count(&self) -> usize {
        self.icons.len()
    }

    /// Handles the events that arrived: dock requests and icons going away.
    /// The strip is arranged again whenever an icon came or went, even when
    /// one replaced another within the batch, which is how Steam docks: it
    /// drops its first icon window and docks a fresh one at once.
    ///
    /// # Returns
    /// Whether the number of icons changed.
    pub fn dispatch(&mut self) -> bool {
        let before = self.icons.len();
        self.changed = false;
        while let Ok(Some(event)) = self.conn.poll_for_event() {
            match event {
                XEvent::ClientMessage(e)
                    if e.window == self.manager && e.type_ == self.atoms._NET_SYSTEM_TRAY_OPCODE =>
                {
                    let data = e.data.as_data32();
                    if data[1] == SYSTEM_TRAY_REQUEST_DOCK && data[2] != x11rb::NONE {
                        self.dock(data[2]);
                    }
                }
                XEvent::DestroyNotify(e) => self.forget(e.window),
                XEvent::ReparentNotify(e) if e.parent != self.holder => self.forget(e.window),
                XEvent::SelectionClear(e) if e.selection == self.selection => {
                    self.icons.clear();
                    self.changed = true;
                }
                _ => {}
            }
        }
        if self.changed {
            self.changed = false;
            self.arrange();
        }
        let _ = self.conn.flush();
        self.icons.len() != before
    }

    /// Embeds an icon window.
    fn dock(&mut self, icon: xproto::Window) {
        if self.icons.contains(&icon) {
            return;
        }
        let _ = self.conn.change_window_attributes(
            icon,
            &xproto::ChangeWindowAttributesAux::new().event_mask(xproto::EventMask::STRUCTURE_NOTIFY),
        );
        let _ = self.conn.change_save_set(xproto::SetMode::INSERT, icon);
        let _ = self.conn.reparent_window(icon, self.holder, 0, 0);
        let notify = xproto::ClientMessageEvent::new(
            32,
            icon,
            self.atoms._XEMBED,
            [x11rb::CURRENT_TIME, XEMBED_EMBEDDED_NOTIFY, 0, self.holder, 0],
        );
        let _ = self
            .conn
            .send_event(false, icon, xproto::EventMask::NO_EVENT, notify);
        let _ = self.conn.map_window(icon);
        self.icons.push(icon);
        self.changed = true;
    }

    /// Drops an icon that went away.
    fn forget(&mut self, window: xproto::Window) {
        if let Some(index) = self.icons.iter().position(|w| *w == window) {
            self.icons.remove(index);
            self.changed = true;
        }
    }

    /// Places the strip over the bar.
    ///
    /// # Arguments
    /// * `slot` - The strip in physical root coordinates, or `None` to hide it.
    /// * `icon_size` - Side of an icon in physical pixels.
    pub fn place(&mut self, slot: Option<Rect>, icon_size: i32) {
        self.slot = slot;
        self.icon_size = icon_size.max(8);
        self.arrange();
        let _ = self.conn.flush();
    }

    /// Lays the icons out in the strip and shows or hides it.
    fn arrange(&mut self) {
        let Some(slot) = self.slot.filter(|_| !self.icons.is_empty()) else {
            if self.mapped {
                let _ = self.conn.unmap_window(self.holder);
                self.mapped = false;
            }
            return;
        };
        let size = self.icon_size;
        let _ = self.conn.configure_window(
            self.holder,
            &xproto::ConfigureWindowAux::new()
                .x(slot.x)
                .y(slot.y)
                .width(slot.w.max(1) as u32)
                .height(slot.h.max(1) as u32)
                .stack_mode(xproto::StackMode::ABOVE),
        );
        for (i, icon) in self.icons.iter().enumerate() {
            let _ = self.conn.configure_window(
                *icon,
                &xproto::ConfigureWindowAux::new()
                    .x(GAP + i as i32 * (size + GAP))
                    .y((slot.h - size) / 2)
                    .width(size as u32)
                    .height(size as u32),
            );
        }
        if !self.mapped {
            let _ = self.conn.map_window(self.holder);
            self.mapped = true;
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        for icon in &self.icons {
            let _ = self.conn.unmap_window(*icon);
            let _ = self.conn.reparent_window(*icon, self.root, 0, 0);
        }
        let _ = self
            .conn
            .set_selection_owner(x11rb::NONE, self.selection, x11rb::CURRENT_TIME);
        let _ = self.conn.destroy_window(self.holder);
        let _ = self.conn.destroy_window(self.manager);
        let _ = self.conn.flush();
    }
}
