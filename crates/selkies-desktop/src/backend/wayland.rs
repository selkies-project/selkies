//! The Wayland backend: layer shell surfaces and xdg-shell windows drawn
//! through shared memory, the window list from the foreign toplevel protocol,
//! file drags through the data device, and the pointer and keyboard of the
//! default seat.  The pointer image is a cursor theme surface
//! at the output's scale, as the compositor's own cursor is, rather than a
//! cursor shape request: a nested compositor forwards a surface to its host
//! with the buffer scale intact, which keeps the size the rest of the desktop
//! has.

use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use wayland_client::backend::ObjectId;
use wayland_client::globals::GlobalListContents;
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source,
    wl_keyboard, wl_output, wl_pointer, wl_region, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop, event_created_child,
};
use wayland_cursor::CursorTheme;
use wayland_protocols::xdg::decoration::zv1::client::{
    zxdg_decoration_manager_v1, zxdg_toplevel_decoration_v1,
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
use wayland_protocols::xdg::toplevel_icon::v1::client::{xdg_toplevel_icon_manager_v1, xdg_toplevel_icon_v1};
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1, zwlr_foreign_toplevel_manager_v1,
};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

use super::Backend;
use crate::desktop::Frame;
use crate::event::{Button, Event, Modifiers, Request, SurfaceId, ToplevelState, WindowAction};
use crate::geom::Rect;
use crate::ui;
use crate::xkb::Keymap;

/// Highest layer shell version the backend speaks.
const LAYER_SHELL_MAX: u32 = 5;
/// Highest foreign toplevel version the backend speaks.
const FOREIGN_TOPLEVEL_MAX: u32 = 3;
/// Highest xdg_wm_base version the backend speaks.
const XDG_WM_BASE_MAX: u32 = 6;
/// Largest surface side taken from the compositor, far past any output.
const MAX_SURFACE_SIDE: i32 = 16384;
/// Largest output scale honored.
const MAX_SCALE: i32 = 4;
/// The mime type file drags are offered as.
const URI_LIST: &str = "text/uri-list";
/// Application ID of the file manager windows.
pub const FILES_APP_ID: &str = "selkies-desktop-files";
/// Linux evdev button codes.
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

/// A shared memory buffer with the mapping the frame is copied into.
struct ShmBuffer {
    pool: wl_shm_pool::WlShmPool,
    buffer: wl_buffer::WlBuffer,
    map: memmap2::MmapMut,
    _fd: OwnedFd,
    width: i32,
    height: i32,
    busy: Arc<AtomicBool>,
    /// Damage of the frames shown through other buffers since this one was
    /// last filled, which the next copy into it must cover as well.
    missed: Vec<crate::geom::Rect>,
}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
    }
}

/// An output with the wallpaper surface placed on it.
struct Output {
    name: u32,
    output: wl_output::WlOutput,
    scale: i32,
    background: Option<Surface>,
    /// The launchers' layer above the wallpaper, where another program's
    /// wallpaper layer stays beneath them.
    launchers: Option<Surface>,
    /// A launchers frame that came before that layer was configured, shown
    /// once it is.
    pending_launchers: Option<PendingFrame>,
}

/// A frame kept until its surface can show it.
struct PendingFrame {
    pixmap: tiny_skia::Pixmap,
    scale: i32,
    input: Vec<Rect>,
}

/// A surface `State::present_target` draws into.
#[derive(Clone, Copy)]
enum Target {
    /// A surface the desktop names.
    Surface(SurfaceId),
    /// The launchers layer of an output, by its number.
    Launchers(u32),
}

/// The user data of an output's launchers layer: the output's number.
struct LauncherLayer(u32);

/// The shell role of a surface.
enum Role {
    /// A layer shell surface: the panel or a wallpaper.
    Layer(zwlr_layer_surface_v1::ZwlrLayerSurfaceV1),
    /// An xdg-shell window the compositor decorates and manages.
    Toplevel {
        xdg: xdg_surface::XdgSurface,
        toplevel: xdg_toplevel::XdgToplevel,
        decoration: Option<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1>,
        /// The size asked for, used while the compositor leaves the choice open.
        wanted: (i32, i32),
    },
}

/// A surface with its buffers.
struct Surface {
    wl: wl_surface::WlSurface,
    role: Role,
    buffers: Vec<ShmBuffer>,
    width: i32,
    height: i32,
    scale: i32,
    configured: bool,
    /// Whether a buffer was attached, so a bare commit keeps the surface
    /// mapped rather than counting as another initial commit.
    mapped: bool,
    /// The input rectangles of the last frame, restored after a drag.
    input: Vec<Rect>,
}

impl Surface {
    /// Releases the role and the surface.
    fn destroy(self) {
        match self.role {
            Role::Layer(layer) => layer.destroy(),
            Role::Toplevel {
                xdg,
                toplevel,
                decoration,
                ..
            } => {
                if let Some(decoration) = decoration {
                    decoration.destroy();
                }
                toplevel.destroy();
                xdg.destroy();
            }
        }
        self.wl.destroy();
    }
}

/// A file manager window.
struct Window {
    id: u32,
    surface: Surface,
    /// The pixels of the title bar icon, kept as long as the window lives.
    _icon: Option<ShmBuffer>,
}

/// A drag in progress, started by the desktop.
struct Drag {
    source: wl_data_source::WlDataSource,
    icon: Option<(wl_surface::WlSurface, ShmBuffer)>,
    uris: Vec<String>,
}

/// A window the compositor reports, with the state gathered until `done`.
struct Handle {
    id: u64,
    handle: zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1,
}

/// The dispatch state behind the event queue.
struct State {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    layer_shell: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    layer_shell_version: u32,
    seat: Option<wl_seat::WlSeat>,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    toplevel_manager: Option<zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    decoration_manager: Option<zxdg_decoration_manager_v1::ZxdgDecorationManagerV1>,
    icon_manager: Option<xdg_toplevel_icon_manager_v1::XdgToplevelIconManagerV1>,
    data_device_manager: Option<wl_data_device_manager::WlDataDeviceManager>,
    data_device: Option<wl_data_device::WlDataDevice>,
    cursor_theme: Option<(i32, CursorTheme)>,
    cursor_surface: Option<wl_surface::WlSurface>,
    conn: Connection,
    outputs: Vec<Output>,
    panel: Option<Surface>,
    windows: Vec<Window>,
    pointer_surface: Option<SurfaceId>,
    pointer_pos: (f64, f64),
    pointer_serial: u32,
    button_serial: u32,
    pointer_shape: bool,
    keyboard_surface: Option<SurfaceId>,
    keymap: Keymap,
    modifiers: Modifiers,
    caps_lock: bool,
    handles: HashMap<ObjectId, Handle>,
    next_toplevel_id: u64,
    events: Vec<Event>,
    popup_open: bool,
    panel_height: i32,
    panel_top: bool,
    panel_autohide: bool,
    drag: Option<Drag>,
    /// The offer of a drag over one of the surfaces, with the surface, the
    /// action the compositor chose for it and the last position reported.
    drag_offer: Option<(wl_data_offer::WlDataOffer, SurfaceId, bool, (f64, f64))>,
}

impl State {
    /// The surface a Wayland surface object stands for; an output's launchers
    /// layer stands for its background.
    fn surface_id(&self, wl: &wl_surface::WlSurface) -> Option<SurfaceId> {
        if self.panel.as_ref().is_some_and(|s| s.wl == *wl) {
            return Some(SurfaceId::Panel);
        }
        if let Some(window) = self.windows.iter().find(|w| w.surface.wl == *wl) {
            return Some(SurfaceId::Window(window.id));
        }
        self.outputs
            .iter()
            .find(|o| {
                o.background.as_ref().is_some_and(|s| s.wl == *wl)
                    || o.launchers.as_ref().is_some_and(|s| s.wl == *wl)
            })
            .map(|o| SurfaceId::Background(o.name))
    }

    /// Mutable access to a surface `present_target` draws into.
    fn target_mut(&mut self, target: Target) -> Option<&mut Surface> {
        match target {
            Target::Surface(id) => self.surface_mut(id),
            Target::Launchers(name) => self
                .outputs
                .iter_mut()
                .find(|o| o.name == name)?
                .launchers
                .as_mut(),
        }
    }

    /// Copies a frame into a free buffer of a surface and commits it; a
    /// launchers frame takes its background's size and scale, and waits until
    /// its layer is configured.
    fn present_target(&mut self, qh: &QueueHandle<State>, target: Target, frame: &Frame<'_>) {
        if let Target::Launchers(name) = target {
            let Some(output) = self.outputs.iter_mut().find(|o| o.name == name) else {
                return;
            };
            let Some((width, height, scale)) =
                output.background.as_ref().map(|b| (b.width, b.height, b.scale))
            else {
                return;
            };
            let Some(launchers) = output.launchers.as_mut() else {
                return;
            };
            if !launchers.configured {
                output.pending_launchers = Some(PendingFrame {
                    pixmap: frame.pixmap.clone(),
                    scale: frame.scale,
                    input: frame.input.clone(),
                });
                return;
            }
            launchers.width = width;
            launchers.height = height;
            launchers.scale = scale;
            output.pending_launchers = None;
        }
        let Some(index) = WaylandBackend::acquire_buffer(self, qh, target) else {
            return;
        };
        let Some(compositor) = self.compositor.clone() else {
            return;
        };
        let dragging = self.drag.is_some();
        let Some(surface) = self.target_mut(target) else {
            return;
        };
        let scale = surface.scale;
        let (surface_w, surface_h) = (surface.width, surface.height);
        for (i, other) in surface.buffers.iter_mut().enumerate() {
            if i != index {
                other.missed.extend(frame.damage.iter().copied());
            }
        }
        let buffer = &mut surface.buffers[index];
        let (bw, bh) = (buffer.width as usize, buffer.height as usize);
        if frame.pixmap.width() as usize != bw || frame.pixmap.height() as usize != bh {
            return;
        }
        let stride = bw * 4;
        let src = frame.pixmap.data();
        let mut to_copy: Vec<crate::geom::Rect> = std::mem::take(&mut buffer.missed);
        to_copy.extend(frame.damage.iter().copied());
        let whole_copy = to_copy.iter().any(|r| r.w >= surface_w && r.h >= surface_h) || to_copy.len() > 32;
        if whole_copy {
            super::copy_bgra(src, &mut buffer.map, stride, 0, 0, bw, bh);
        } else {
            for rect in &to_copy {
                let r = rect
                    .scaled(scale)
                    .intersect(&crate::geom::Rect::new(0, 0, bw as i32, bh as i32));
                if !r.is_empty() {
                    super::copy_bgra(
                        src,
                        &mut buffer.map,
                        stride,
                        r.x as usize,
                        r.y as usize,
                        r.w as usize,
                        r.h as usize,
                    );
                }
            }
        }
        buffer.busy.store(true, Ordering::Release);

        surface.input = frame.input.clone();
        let buffer = &surface.buffers[index];
        if !(dragging && matches!(target, Target::Launchers(_))) {
            let region = compositor.create_region(qh, ());
            for rect in &frame.input {
                region.add(rect.x, rect.y, rect.w, rect.h);
            }
            surface.wl.set_input_region(Some(&region));
            region.destroy();
        }
        surface.wl.set_buffer_scale(scale);
        surface.wl.attach(Some(&buffer.buffer), 0, 0);
        for rect in &frame.damage {
            let r = rect.scaled(scale);
            surface.wl.damage_buffer(r.x, r.y, r.w, r.h);
        }
        surface.wl.commit();
        surface.mapped = true;
    }

    /// Mutable access to a surface by identifier.
    fn surface_mut(&mut self, id: SurfaceId) -> Option<&mut Surface> {
        match id {
            SurfaceId::Panel => self.panel.as_mut(),
            SurfaceId::Background(name) => self
                .outputs
                .iter_mut()
                .find(|o| o.name == name)?
                .background
                .as_mut(),
            SurfaceId::Window(id) => self
                .windows
                .iter_mut()
                .find(|w| w.id == id)
                .map(|w| &mut w.surface),
        }
    }

    /// Creates a file manager window of a size, left to the compositor to
    /// place, with the icon its title bar shows: the file manager's icon name
    /// for the compositor's theme, and our own rendering of it as pixels for
    /// a compositor that cannot resolve the name.
    fn create_window(
        &mut self,
        qh: &QueueHandle<State>,
        id: u32,
        title: &str,
        width: i32,
        height: i32,
        icon: Option<&tiny_skia::Pixmap>,
    ) {
        let (Some(compositor), Some(wm_base)) = (self.compositor.clone(), self.wm_base.clone()) else {
            return;
        };
        if self.windows.iter().any(|w| w.id == id) {
            return;
        }
        let wl = compositor.create_surface(qh, ());
        let xdg = wm_base.get_xdg_surface(&wl, qh, SurfaceId::Window(id));
        let toplevel = xdg.get_toplevel(qh, SurfaceId::Window(id));
        toplevel.set_app_id(FILES_APP_ID.to_string());
        toplevel.set_title(title.to_string());
        toplevel.set_min_size(ui::WINDOW_MIN_WIDTH, ui::WINDOW_MIN_HEIGHT);
        let decoration = self.decoration_manager.as_ref().map(|manager| {
            let decoration = manager.get_toplevel_decoration(&toplevel, qh, ());
            decoration.set_mode(zxdg_toplevel_decoration_v1::Mode::ServerSide);
            decoration
        });
        let mut icon_buffer = None;
        if let Some(manager) = &self.icon_manager {
            let icon_object = manager.create_icon(qh, ());
            icon_object.set_name("system-file-manager".to_string());
            if let Some(pixmap) = icon
                && let Some(shm) = &self.shm
                && pixmap.width() == pixmap.height()
                && let Some(buffer) = single_buffer(shm, qh, pixmap)
            {
                icon_object.add_buffer(&buffer.buffer, 1);
                icon_buffer = Some(buffer);
            }
            manager.set_icon(&toplevel, Some(&icon_object));
            icon_object.destroy();
        }
        wl.commit();
        let scale = self.outputs.first().map_or(1, |o| o.scale);
        self.windows.push(Window {
            id,
            _icon: icon_buffer,
            surface: Surface {
                wl,
                role: Role::Toplevel {
                    xdg,
                    toplevel,
                    decoration,
                    wanted: (width, height),
                },
                buffers: Vec::new(),
                width: 0,
                height: 0,
                scale,
                configured: false,
                mapped: false,
                input: Vec::new(),
            },
        });
    }

    /// Destroys a file manager window.
    fn destroy_window(&mut self, id: u32) {
        if let Some(index) = self.windows.iter().position(|w| w.id == id) {
            let window = self.windows.remove(index);
            window.surface.destroy();
        }
        if self.pointer_surface == Some(SurfaceId::Window(id)) {
            self.pointer_surface = None;
        }
        if self.keyboard_surface == Some(SurfaceId::Window(id)) {
            self.keyboard_surface = None;
        }
    }

    /// Starts a drag of file URIs from a window, with the ghost image as the
    /// drag icon.  The wallpaper surfaces take input everywhere while it lasts
    /// so a drop on empty desktop reaches them.
    fn start_drag(
        &mut self,
        qh: &QueueHandle<State>,
        window: u32,
        uris: Vec<String>,
        icon: Option<&tiny_skia::Pixmap>,
        scale: i32,
    ) {
        let (Some(manager), Some(device), Some(compositor), Some(shm)) = (
            self.data_device_manager.clone(),
            self.data_device.clone(),
            self.compositor.clone(),
            self.shm.clone(),
        ) else {
            self.events.push(Event::DragEnd);
            return;
        };
        let Some(origin) = self
            .windows
            .iter()
            .find(|w| w.id == window)
            .map(|w| w.surface.wl.clone())
        else {
            self.events.push(Event::DragEnd);
            return;
        };
        if let Some(previous) = self.drag.take() {
            previous.source.destroy();
        }
        let source = manager.create_data_source(qh, ());
        source.offer(URI_LIST.to_string());
        if manager.version() >= 3 {
            source.set_actions(
                wl_data_device_manager::DndAction::Move | wl_data_device_manager::DndAction::Copy,
            );
        }
        let icon = icon.and_then(|pixmap| {
            let buffer = single_buffer(&shm, qh, pixmap)?;
            let surface = compositor.create_surface(qh, ());
            surface.set_buffer_scale(scale);
            surface.attach(Some(&buffer.buffer), 0, 0);
            surface.damage_buffer(0, 0, buffer.width, buffer.height);
            surface.commit();
            Some((surface, buffer))
        });
        device.start_drag(
            Some(&source),
            &origin,
            icon.as_ref().map(|(surface, _)| surface),
            self.button_serial,
        );
        self.drag = Some(Drag { source, icon, uris });
        for output in &self.outputs {
            if let Some(launchers) = output.launchers.as_ref().filter(|s| s.mapped) {
                launchers.wl.set_input_region(None);
                launchers.wl.commit();
            }
        }
    }

    /// Ends the drag: releases its objects and gives the wallpapers their
    /// input regions back.
    fn finish_drag(&mut self, qh: &QueueHandle<State>) {
        let Some(drag) = self.drag.take() else {
            return;
        };
        drag.source.destroy();
        if let Some((surface, buffer)) = drag.icon {
            surface.destroy();
            drop(buffer);
        }
        if let Some(compositor) = self.compositor.clone() {
            for output in &self.outputs {
                if let Some(launchers) = output.launchers.as_ref().filter(|s| s.mapped) {
                    let region = compositor.create_region(qh, ());
                    for rect in &launchers.input {
                        region.add(rect.x, rect.y, rect.w, rect.h);
                    }
                    launchers.wl.set_input_region(Some(&region));
                    region.destroy();
                    launchers.wl.commit();
                }
            }
        }
        self.events.push(Event::DragEnd);
    }

    /// The output the panel sits on, the first one the compositor announced.
    fn primary_output(&self) -> Option<u32> {
        self.outputs.first().map(|o| o.name)
    }

    /// Emits the configured size and scale of a surface to the desktop, and
    /// for a background the monitor it spans.
    fn emit_configured(&mut self, id: SurfaceId) {
        let primary = self.primary_output();
        if let Some(surface) = self.surface_mut(id)
            && surface.configured
        {
            let (width, height, scale) = (surface.width, surface.height, surface.scale);
            self.events.push(Event::Configured {
                surface: id,
                width,
                height,
                scale,
            });
            if let SurfaceId::Background(name) = id {
                self.events.push(Event::BackgroundLayout {
                    surface: name,
                    monitors: vec![crate::geom::Rect::new(0, 0, width, height)],
                    primary: primary == Some(name),
                });
            }
        }
    }

    /// Applies an output's scale to the surfaces on it.
    fn apply_scale(&mut self, output: &ObjectId, factor: i32) {
        let scale = factor.clamp(1, MAX_SCALE);
        let Some(index) = self.outputs.iter().position(|o| o.output.id() == *output) else {
            return;
        };
        self.outputs[index].scale = scale;
        let name = self.outputs[index].name;
        let mut ids = vec![SurfaceId::Background(name)];
        if index == 0 {
            ids.push(SurfaceId::Panel);
        }
        for id in ids {
            if let Some(surface) = self.surface_mut(id)
                && surface.scale != scale
            {
                surface.scale = scale;
                self.emit_configured(id);
            }
        }
    }

    /// Registers an output and, once the globals are bound, its wallpaper surface.
    fn add_output(&mut self, qh: &QueueHandle<State>, name: u32, output: wl_output::WlOutput) {
        if self.outputs.iter().any(|o| o.name == name) {
            return;
        }
        self.outputs.push(Output {
            name,
            output,
            scale: 1,
            background: None,
            launchers: None,
            pending_launchers: None,
        });
        self.create_background(qh, name);
    }

    /// Creates the wallpaper layer surface of an output and the launchers
    /// layer above it.
    fn create_background(&mut self, qh: &QueueHandle<State>, name: u32) {
        let (Some(compositor), Some(layer_shell)) = (self.compositor.clone(), self.layer_shell.clone())
        else {
            return;
        };
        let Some(output) = self.outputs.iter_mut().find(|o| o.name == name) else {
            return;
        };
        if output.background.is_some() {
            return;
        }
        let wl = compositor.create_surface(qh, ());
        let layer = layer_shell.get_layer_surface(
            &wl,
            Some(&output.output),
            zwlr_layer_shell_v1::Layer::Background,
            "wallpaper".to_string(),
            qh,
            SurfaceId::Background(name),
        );
        layer.set_anchor(
            zwlr_layer_surface_v1::Anchor::Top
                | zwlr_layer_surface_v1::Anchor::Bottom
                | zwlr_layer_surface_v1::Anchor::Left
                | zwlr_layer_surface_v1::Anchor::Right,
        );
        layer.set_size(0, 0);
        layer.set_exclusive_zone(-1);
        wl.commit();
        output.background = Some(Surface {
            wl,
            role: Role::Layer(layer),
            buffers: Vec::new(),
            width: 0,
            height: 0,
            scale: output.scale,
            configured: false,
            mapped: false,
            input: Vec::new(),
        });
        let wl = compositor.create_surface(qh, ());
        let layer = layer_shell.get_layer_surface(
            &wl,
            Some(&output.output),
            zwlr_layer_shell_v1::Layer::Bottom,
            "desktop".to_string(),
            qh,
            LauncherLayer(name),
        );
        layer.set_anchor(
            zwlr_layer_surface_v1::Anchor::Top
                | zwlr_layer_surface_v1::Anchor::Bottom
                | zwlr_layer_surface_v1::Anchor::Left
                | zwlr_layer_surface_v1::Anchor::Right,
        );
        layer.set_size(0, 0);
        layer.set_exclusive_zone(-1);
        wl.commit();
        output.launchers = Some(Surface {
            wl,
            role: Role::Layer(layer),
            buffers: Vec::new(),
            width: 0,
            height: 0,
            scale: output.scale,
            configured: false,
            mapped: false,
            input: Vec::new(),
        });
    }

    /// Forgets an output the compositor removed, with its wallpaper surface.
    fn remove_output(&mut self, name: u32) {
        let Some(index) = self.outputs.iter().position(|o| o.name == name) else {
            return;
        };
        let output = self.outputs.remove(index);
        if let Some(surface) = output.background {
            surface.destroy();
        }
        if let Some(surface) = output.launchers {
            surface.destroy();
        }
        output.output.release();
        self.events.push(Event::BackgroundRemoved { surface: name });
    }

    /// Shows the pointer image for the hovered content on the entered surface.
    fn update_cursor(&mut self) {
        let Some(pointer) = self.pointer.clone() else {
            return;
        };
        let Some(cursor_surface) = self.cursor_surface.clone() else {
            return;
        };
        let scale = match self.pointer_surface {
            Some(SurfaceId::Panel) => self.panel.as_ref().map_or(1, |s| s.scale),
            Some(SurfaceId::Background(name)) => self
                .outputs
                .iter()
                .find(|o| o.name == name)
                .map_or(1, |o| o.scale),
            Some(SurfaceId::Window(id)) => self
                .windows
                .iter()
                .find(|w| w.id == id)
                .map_or(1, |w| w.surface.scale),
            None => 1,
        };
        if self
            .cursor_theme
            .as_ref()
            .is_none_or(|(loaded, _)| *loaded != scale)
            && let Some(shm) = self.shm.clone()
        {
            let size = std::env::var("XCURSOR_SIZE")
                .ok()
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(24);
            self.cursor_theme = CursorTheme::load_or(&self.conn, shm, "default", size * scale as u32)
                .ok()
                .map(|theme| (scale, theme));
        }
        let names: &[&str] = if self.pointer_shape {
            &["pointer", "hand2", "hand1", "left_ptr"]
        } else {
            &["left_ptr", "default", "arrow"]
        };
        let Some((_, theme)) = self.cursor_theme.as_mut() else {
            return;
        };
        let Some(name) = names.iter().find(|name| theme.get_cursor(name).is_some()) else {
            return;
        };
        let Some(cursor) = theme.get_cursor(name) else {
            return;
        };
        let image = &cursor[0];
        let (w, h) = image.dimensions();
        let (hx, hy) = image.hotspot();
        cursor_surface.set_buffer_scale(scale);
        cursor_surface.attach(Some(image), 0, 0);
        cursor_surface.damage_buffer(0, 0, w as i32, h as i32);
        cursor_surface.commit();
        pointer.set_cursor(
            self.pointer_serial,
            Some(&cursor_surface),
            hx as i32 / scale,
            hy as i32 / scale,
        );
    }
}

/// The Wayland backend.
pub struct WaylandBackend {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    read_guard: Option<wayland_client::backend::ReadEventsGuard>,
}

impl WaylandBackend {
    /// Connects to the compositor and creates the panel and background layer
    /// surfaces.  Fails without side effects when there is no compositor or
    /// it lacks layer shell support, so the X11 backend can be tried.
    ///
    /// # Errors
    /// Why the connection cannot be used.
    pub fn connect() -> Result<WaylandBackend, String> {
        let conn = Connection::connect_to_env().map_err(|e| format!("{e}"))?;
        let (globals, mut queue) =
            wayland_client::globals::registry_queue_init::<State>(&conn).map_err(|e| format!("{e}"))?;
        let qh = queue.handle();
        let mut state = State {
            compositor: None,
            shm: None,
            layer_shell: None,
            layer_shell_version: 1,
            seat: None,
            pointer: None,
            keyboard: None,
            toplevel_manager: None,
            wm_base: None,
            decoration_manager: None,
            icon_manager: None,
            data_device_manager: None,
            data_device: None,
            cursor_theme: None,
            cursor_surface: None,
            conn: conn.clone(),
            outputs: Vec::new(),
            panel: None,
            windows: Vec::new(),
            pointer_surface: None,
            pointer_pos: (0.0, 0.0),
            pointer_serial: 0,
            button_serial: 0,
            pointer_shape: false,
            keyboard_surface: None,
            keymap: Keymap::default(),
            modifiers: Modifiers::default(),
            caps_lock: false,
            handles: HashMap::new(),
            next_toplevel_id: 1,
            events: Vec::new(),
            popup_open: false,
            panel_height: ui::PANEL_HEIGHT,
            panel_top: false,
            panel_autohide: false,
            drag: None,
            drag_offer: None,
        };

        let mut layer_shell_version = 0;
        let mut toplevel_version = 0;
        let mut wm_base_version = 0;
        let mut decoration = false;
        let mut icon_manager = false;
        let mut data_device_version = 0;
        globals.contents().with_list(|list| {
            for global in list {
                match global.interface.as_str() {
                    "zwlr_layer_shell_v1" => layer_shell_version = global.version.min(LAYER_SHELL_MAX),
                    "zwlr_foreign_toplevel_manager_v1" => {
                        toplevel_version = global.version.min(FOREIGN_TOPLEVEL_MAX)
                    }
                    "xdg_wm_base" => wm_base_version = global.version.min(XDG_WM_BASE_MAX),
                    "zxdg_decoration_manager_v1" => decoration = true,
                    "xdg_toplevel_icon_manager_v1" => icon_manager = true,
                    "wl_data_device_manager" => data_device_version = global.version.min(3),
                    _ => {}
                }
            }
        });
        if layer_shell_version == 0 {
            return Err("compositor has no layer shell".to_string());
        }
        state.compositor = Some(
            globals
                .bind::<wl_compositor::WlCompositor, _, _>(&qh, 4..=6, ())
                .map_err(|e| format!("{e}"))?,
        );
        state.shm = Some(
            globals
                .bind::<wl_shm::WlShm, _, _>(&qh, 1..=1, ())
                .map_err(|e| format!("{e}"))?,
        );
        state.layer_shell = Some(
            globals
                .bind::<zwlr_layer_shell_v1::ZwlrLayerShellV1, _, _>(&qh, 1..=layer_shell_version, ())
                .map_err(|e| format!("{e}"))?,
        );
        state.layer_shell_version = layer_shell_version;
        state.seat = globals.bind::<wl_seat::WlSeat, _, _>(&qh, 1..=5, ()).ok();
        if toplevel_version > 0 {
            state.toplevel_manager = globals
                .bind::<zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1, _, _>(
                    &qh,
                    1..=toplevel_version,
                    (),
                )
                .ok();
        }
        if wm_base_version > 0 {
            state.wm_base = globals
                .bind::<xdg_wm_base::XdgWmBase, _, _>(&qh, 1..=wm_base_version, ())
                .ok();
        }
        if decoration {
            state.decoration_manager = globals
                .bind::<zxdg_decoration_manager_v1::ZxdgDecorationManagerV1, _, _>(&qh, 1..=1, ())
                .ok();
        }
        if icon_manager {
            state.icon_manager = globals
                .bind::<xdg_toplevel_icon_manager_v1::XdgToplevelIconManagerV1, _, _>(&qh, 1..=1, ())
                .ok();
        }
        if data_device_version > 0 {
            state.data_device_manager = globals
                .bind::<wl_data_device_manager::WlDataDeviceManager, _, _>(&qh, 1..=data_device_version, ())
                .ok();
            if let (Some(manager), Some(seat)) = (&state.data_device_manager, &state.seat) {
                state.data_device = Some(manager.get_data_device(seat, &qh, ()));
            }
        }
        let mut announced: Vec<(u32, u32)> = Vec::new();
        globals.contents().with_list(|list| {
            for global in list {
                if global.interface == "wl_output" {
                    announced.push((global.name, global.version));
                }
            }
        });
        announced.sort();
        for (name, version) in announced {
            let output =
                globals
                    .registry()
                    .bind::<wl_output::WlOutput, _, _>(name, version.clamp(2, 4), &qh, ());
            state.add_output(&qh, name, output);
        }

        let compositor = state.compositor.clone().expect("bound");
        let layer_shell = state.layer_shell.clone().expect("bound");

        let primary = state.outputs.first().map(|o| o.output.clone());
        let panel_wl = compositor.create_surface(&qh, ());
        let panel_layer = layer_shell.get_layer_surface(
            &panel_wl,
            primary.as_ref(),
            zwlr_layer_shell_v1::Layer::Bottom,
            "panel".to_string(),
            &qh,
            SurfaceId::Panel,
        );
        anchor_panel(&panel_layer, layer_shell_version, false, ui::PANEL_HEIGHT);
        panel_wl.commit();

        state.panel = Some(Surface {
            wl: panel_wl,
            role: Role::Layer(panel_layer),
            buffers: Vec::new(),
            width: 0,
            height: 0,
            scale: 1,
            configured: false,
            mapped: false,
            input: Vec::new(),
        });

        state.cursor_surface = Some(compositor.create_surface(&qh, ()));
        let _ = layer_shell;

        queue.roundtrip(&mut state).map_err(|e| format!("{e}"))?;
        queue.roundtrip(&mut state).map_err(|e| format!("{e}"))?;
        Ok(WaylandBackend {
            conn,
            queue,
            state,
            read_guard: None,
        })
    }

    /// Finds or creates a free buffer of the surface's size.
    fn acquire_buffer(state: &mut State, qh: &QueueHandle<State>, target: Target) -> Option<usize> {
        let shm = state.shm.clone()?;
        let surface = state.target_mut(target)?;
        let width = surface.width * surface.scale;
        let height = surface.height * surface.scale;
        if width < 1 || height < 1 || width > MAX_SURFACE_SIDE || height > MAX_SURFACE_SIDE {
            return None;
        }
        surface.buffers.retain(|b| b.width == width && b.height == height);
        if let Some(index) = surface
            .buffers
            .iter()
            .position(|b| !b.busy.load(Ordering::Acquire))
        {
            return Some(index);
        }
        if surface.buffers.len() >= 3 {
            return Some(0);
        }
        let size = width as usize * height as usize * 4;
        let fd = create_memfd(size)?;
        let map = unsafe { memmap2::MmapMut::map_mut(&fd) }.ok()?;
        let pool = shm.create_pool(fd.as_fd(), size as i32, qh, ());
        let busy = Arc::new(AtomicBool::new(false));
        let buffer = pool.create_buffer(
            0,
            width,
            height,
            width * 4,
            wl_shm::Format::Argb8888,
            qh,
            busy.clone(),
        );
        surface.buffers.push(ShmBuffer {
            pool,
            buffer,
            map,
            _fd: fd,
            width,
            height,
            busy,
            missed: Vec::new(),
        });
        Some(surface.buffers.len() - 1)
    }
}

/// Allocates one shared memory buffer holding a pixmap, for the drag icon.
fn single_buffer(
    shm: &wl_shm::WlShm,
    qh: &QueueHandle<State>,
    pixmap: &tiny_skia::Pixmap,
) -> Option<ShmBuffer> {
    let (width, height) = (pixmap.width() as i32, pixmap.height() as i32);
    if width < 1 || height < 1 {
        return None;
    }
    let size = width as usize * height as usize * 4;
    let fd = create_memfd(size)?;
    let mut map = unsafe { memmap2::MmapMut::map_mut(&fd) }.ok()?;
    super::copy_bgra(
        pixmap.data(),
        &mut map,
        width as usize * 4,
        0,
        0,
        width as usize,
        height as usize,
    );
    let pool = shm.create_pool(fd.as_fd(), size as i32, qh, ());
    let busy = Arc::new(AtomicBool::new(false));
    let buffer = pool.create_buffer(
        0,
        width,
        height,
        width * 4,
        wl_shm::Format::Argb8888,
        qh,
        busy.clone(),
    );
    Some(ShmBuffer {
        pool,
        buffer,
        map,
        _fd: fd,
        width,
        height,
        busy,
        missed: Vec::new(),
    })
}

/// Anchors the panel's layer surface along the bottom or top edge and
/// reserves the bar's height.  With layer shell v5 the surface covers the
/// output and names its exclusive edge, so a click anywhere reaches it while
/// a popup is open; older compositors get a strip the height of the bar plus
/// the popup area.
fn anchor_panel(layer: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, version: u32, top: bool, height: i32) {
    layer.set_exclusive_zone(height);
    let edge = if top {
        zwlr_layer_surface_v1::Anchor::Top
    } else {
        zwlr_layer_surface_v1::Anchor::Bottom
    };
    if version >= 5 {
        layer.set_anchor(
            zwlr_layer_surface_v1::Anchor::Top
                | zwlr_layer_surface_v1::Anchor::Bottom
                | zwlr_layer_surface_v1::Anchor::Left
                | zwlr_layer_surface_v1::Anchor::Right,
        );
        layer.set_size(0, 0);
        layer.set_exclusive_edge(edge);
    } else {
        layer.set_anchor(edge | zwlr_layer_surface_v1::Anchor::Left | zwlr_layer_surface_v1::Anchor::Right);
        layer.set_size(0, (height + ui::POPUP_AREA_HEIGHT) as u32);
    }
}

/// Allocates an anonymous shared memory file of a size.
fn create_memfd(size: usize) -> Option<OwnedFd> {
    use std::os::fd::FromRawFd;
    let name = c"selkies-desktop";
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
    if fd < 0 {
        return None;
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    if unsafe { libc::ftruncate(fd.as_raw_fd(), size as libc::off_t) } != 0 {
        return None;
    }
    unsafe {
        libc::fcntl(
            fd.as_raw_fd(),
            libc::F_ADD_SEALS,
            libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL,
        );
    }
    Some(fd)
}

impl Backend for WaylandBackend {
    fn name(&self) -> &'static str {
        "wayland"
    }

    fn fd(&self) -> RawFd {
        self.conn.as_fd().as_raw_fd()
    }

    fn prepare(&mut self) -> Result<(), String> {
        loop {
            match self.conn.prepare_read() {
                Some(guard) => {
                    self.read_guard = Some(guard);
                    break;
                }
                None => {
                    self.queue
                        .dispatch_pending(&mut self.state)
                        .map_err(|e| format!("{e}"))?;
                }
            }
        }
        match self.conn.flush() {
            Ok(()) => Ok(()),
            Err(wayland_client::backend::WaylandError::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock =>
            {
                Ok(())
            }
            Err(e) => Err(format!("{e}")),
        }
    }

    fn pending(&self) -> bool {
        !self.state.events.is_empty()
    }

    fn panel_origin(&self) -> (i32, i32, i32) {
        let Some(panel) = self.state.panel.as_ref() else {
            return (0, 0, 1);
        };
        let output_height = self
            .state
            .outputs
            .first()
            .and_then(|o| o.background.as_ref())
            .map_or(panel.height, |b| b.height);
        let y = if self.state.panel_top {
            0
        } else {
            output_height - panel.height
        };
        (0, y, 1)
    }

    fn dispatch(&mut self, readable: bool) -> Result<Vec<Event>, String> {
        if let Some(guard) = self.read_guard.take()
            && readable
        {
            match guard.read() {
                Ok(_) => {}
                Err(wayland_client::backend::WaylandError::Io(e))
                    if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(format!("{e}")),
            }
        }
        self.queue
            .dispatch_pending(&mut self.state)
            .map_err(|e| format!("{e}"))?;
        if let Some(error) = self.conn.protocol_error() {
            return Err(format!("protocol error: {}", error.message));
        }
        Ok(std::mem::take(&mut self.state.events))
    }

    fn present(&mut self, id: SurfaceId, frame: &Frame<'_>) {
        let qh = self.queue.handle();
        self.state.present_target(&qh, Target::Surface(id), frame);
    }

    fn layered_desktop(&self) -> bool {
        true
    }

    fn present_launchers(&mut self, background: u32, frame: &Frame<'_>) {
        let qh = self.queue.handle();
        self.state
            .present_target(&qh, Target::Launchers(background), frame);
    }

    fn perform(&mut self, request: &Request) {
        match request {
            Request::Window { id, action } => {
                let Some(handle) = self.state.handles.values().find(|h| h.id == *id) else {
                    return;
                };
                match action {
                    WindowAction::Activate => {
                        if let Some(seat) = &self.state.seat {
                            handle.handle.activate(seat);
                        }
                    }
                    WindowAction::Minimize => handle.handle.set_minimized(),
                    WindowAction::Unminimize => handle.handle.unset_minimized(),
                    WindowAction::Maximize => handle.handle.set_maximized(),
                    WindowAction::Unmaximize => handle.handle.unset_maximized(),
                    WindowAction::Close => handle.handle.close(),
                }
            }
            Request::MenuOpen(open) => {
                self.state.popup_open = *open;
                let version = self.state.layer_shell_version;
                if let Some(Surface {
                    role: Role::Layer(layer),
                    ..
                }) = &self.state.panel
                {
                    if version >= 2 {
                        layer.set_layer(if *open || self.state.panel_autohide {
                            zwlr_layer_shell_v1::Layer::Top
                        } else {
                            zwlr_layer_shell_v1::Layer::Bottom
                        });
                    }
                    let interactivity = if *open {
                        zwlr_layer_surface_v1::KeyboardInteractivity::Exclusive
                    } else {
                        zwlr_layer_surface_v1::KeyboardInteractivity::None
                    };
                    layer.set_keyboard_interactivity(interactivity);
                }
            }
            Request::Cursor { pointer } => {
                if self.state.pointer_shape != *pointer {
                    self.state.pointer_shape = *pointer;
                    if self.state.pointer_surface.is_some() {
                        self.state.update_cursor();
                    }
                }
            }
            Request::Panel {
                top,
                height,
                autohide,
                margin: _,
            } => {
                self.state.panel_height = *height;
                self.state.panel_top = *top;
                self.state.panel_autohide = *autohide;
                if let Some(Surface {
                    role: Role::Layer(layer),
                    wl,
                    mapped,
                    ..
                }) = &self.state.panel
                {
                    anchor_panel(layer, self.state.layer_shell_version, *top, *height);
                    if *autohide {
                        layer.set_exclusive_zone(0);
                    }
                    if self.state.layer_shell_version >= 2 {
                        layer.set_layer(if *autohide || self.state.popup_open {
                            zwlr_layer_shell_v1::Layer::Top
                        } else {
                            zwlr_layer_shell_v1::Layer::Bottom
                        });
                    }
                    if *mapped {
                        wl.commit();
                    }
                }
            }
            Request::OpenWindow {
                id,
                title,
                width,
                height,
                icon,
            } => {
                let qh = self.queue.handle();
                self.state
                    .create_window(&qh, *id, title, *width, *height, icon.as_deref());
            }
            Request::CloseWindow { id } => self.state.destroy_window(*id),
            Request::SetTitle { id, title } => {
                if let Some(Window {
                    surface:
                        Surface {
                            role: Role::Toplevel { toplevel, .. },
                            ..
                        },
                    ..
                }) = self.state.windows.iter().find(|w| w.id == *id)
                {
                    toplevel.set_title(title.clone());
                }
            }
            Request::StartDrag {
                window,
                uris,
                icon,
                scale,
            } => {
                let qh = self.queue.handle();
                self.state
                    .start_drag(&qh, *window, uris.clone(), icon.as_deref(), *scale);
            }
            Request::SetVolume(_) | Request::ToggleMute => {}
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        wm_base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, SurfaceId> for State {
    fn event(
        state: &mut Self,
        xdg: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        id: &SurfaceId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg.ack_configure(serial);
            if let Some(surface) = state.surface_mut(*id) {
                if (surface.width <= 0 || surface.height <= 0)
                    && let Role::Toplevel { wanted, .. } = &surface.role
                {
                    surface.width = wanted.0;
                    surface.height = wanted.1;
                }
                surface.configured = surface.width > 0 && surface.height > 0;
            }
            state.emit_configured(*id);
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, SurfaceId> for State {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        id: &SurfaceId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Configure { width, height, .. } => {
                if let Some(surface) = state.surface_mut(*id) {
                    if width > 0 && height > 0 {
                        surface.width = width.min(MAX_SURFACE_SIDE);
                        surface.height = height.min(MAX_SURFACE_SIDE);
                    } else if let Role::Toplevel { wanted, .. } = &surface.role {
                        surface.width = wanted.0;
                        surface.height = wanted.1;
                    }
                }
            }
            xdg_toplevel::Event::Close => {
                if let SurfaceId::Window(window) = id {
                    state.events.push(Event::WindowCloseRequested { window: *window });
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_data_device::WlDataDevice, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_data_device::WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_device::Event::Enter {
                serial,
                surface,
                x,
                y,
                id,
            } => {
                let Some(target) = state.surface_id(&surface) else {
                    return;
                };
                if let Some((offer, ..)) = state.drag_offer.take() {
                    offer.destroy();
                }
                if let Some(offer) = id {
                    offer.accept(serial, Some(URI_LIST.to_string()));
                    if offer.version() >= 3 {
                        offer.set_actions(
                            wl_data_device_manager::DndAction::Move | wl_data_device_manager::DndAction::Copy,
                            wl_data_device_manager::DndAction::Move,
                        );
                    }
                    state.drag_offer = Some((offer, target, false, (x, y)));
                }
                if state.drag.is_some() {
                    state.events.push(Event::DragEnter {
                        surface: target,
                        x,
                        y,
                    });
                }
            }
            wl_data_device::Event::Motion { x, y, .. } => {
                if let Some((_, target, _, position)) = state.drag_offer.as_mut() {
                    *position = (x, y);
                    let surface = *target;
                    if state.drag.is_some() {
                        state.events.push(Event::DragMotion { surface, x, y });
                    }
                }
            }
            wl_data_device::Event::Leave => {
                if let Some((offer, target, _, _)) = state.drag_offer.take() {
                    offer.destroy();
                    if state.drag.is_some() {
                        state.events.push(Event::DragLeave { surface: target });
                    }
                }
            }
            wl_data_device::Event::Drop => {
                if let Some((offer, target, copy, (x, y))) = state.drag_offer.take() {
                    if state.drag.is_some() {
                        state.events.push(Event::DragDrop {
                            surface: target,
                            x,
                            y,
                            copy: copy || state.modifiers.ctrl,
                        });
                    }
                    if offer.version() >= 3 {
                        offer.finish();
                    }
                    offer.destroy();
                }
            }
            wl_data_device::Event::Selection { id: Some(offer) } => offer.destroy(),
            _ => {}
        }
    }

    event_created_child!(State, wl_data_device::WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (wl_data_offer::WlDataOffer, ()),
    ]);
}

impl Dispatch<wl_data_offer::WlDataOffer, ()> for State {
    fn event(
        state: &mut Self,
        offer: &wl_data_offer::WlDataOffer,
        event: wl_data_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_data_offer::Event::Action {
            dnd_action: WEnum::Value(action),
        } = event
            && let Some((current, _, copy, _)) = state.drag_offer.as_mut()
            && current == offer
        {
            *copy = action == wl_data_device_manager::DndAction::Copy;
        }
    }
}

impl Dispatch<wl_data_source::WlDataSource, ()> for State {
    fn event(
        state: &mut Self,
        source: &wl_data_source::WlDataSource,
        event: wl_data_source::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let ours = state.drag.as_ref().is_some_and(|d| d.source == *source);
        match event {
            wl_data_source::Event::Send { mime_type, fd } => {
                if ours && mime_type == URI_LIST {
                    let text: String = state
                        .drag
                        .as_ref()
                        .map(|d| d.uris.iter().map(|u| format!("{u}\r\n")).collect())
                        .unwrap_or_default();
                    let mut file = std::fs::File::from(fd);
                    use std::io::Write as _;
                    let _ = file.write_all(text.as_bytes());
                }
            }
            wl_data_source::Event::Cancelled | wl_data_source::Event::DndFinished => {
                if ours {
                    state.finish_drag(qh);
                } else {
                    source.destroy();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == "wl_output" => {
                let output = registry.bind::<wl_output::WlOutput, _, _>(name, version.clamp(2, 4), qh, ());
                state.add_output(qh, name, output);
            }
            wl_registry::Event::GlobalRemove { name } => state.remove_output(name),
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Scale { factor } = event
            && factor > 0
        {
            state.apply_scale(&output.id(), factor);
        }
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for State {
    fn event(
        state: &mut Self,
        surface: &wl_surface::WlSurface,
        event: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(id) = state.surface_id(surface) else {
            return;
        };
        match event {
            wl_surface::Event::PreferredBufferScale { factor } if factor > 0 => {
                if let Some(s) = state.surface_mut(id)
                    && s.scale != factor.clamp(1, MAX_SCALE)
                {
                    s.scale = factor.clamp(1, MAX_SCALE);
                    state.emit_configured(id);
                }
            }
            wl_surface::Event::Enter { output } => {
                let scale = state
                    .outputs
                    .iter()
                    .find(|o| o.output.id() == output.id())
                    .map_or(1, |o| o.scale)
                    .clamp(1, MAX_SCALE);
                if let Some(s) = state.surface_mut(id)
                    && s.scale != scale
                {
                    s.scale = scale;
                    state.emit_configured(id);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, SurfaceId> for State {
    fn event(
        state: &mut Self,
        layer: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        id: &SurfaceId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                layer.ack_configure(serial);
                let (width, height) = (width as i32, height as i32);
                if let Some(surface) = state.surface_mut(*id) {
                    if width > 0 {
                        surface.width = width.min(MAX_SURFACE_SIDE);
                    }
                    if height > 0 {
                        surface.height = height.min(MAX_SURFACE_SIDE);
                    }
                    surface.configured = surface.width > 0 && surface.height > 0;
                }
                state.emit_configured(*id);
            }
            zwlr_layer_surface_v1::Event::Closed => {
                state.events.push(Event::Quit);
            }
            _ => {}
        }
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, LauncherLayer> for State {
    fn event(
        state: &mut Self,
        layer: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        data: &LauncherLayer,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                layer.ack_configure(serial);
                let (width, height) = (width as i32, height as i32);
                let Some(output) = state.outputs.iter_mut().find(|o| o.name == data.0) else {
                    return;
                };
                if let Some(surface) = output.launchers.as_mut() {
                    if width > 0 {
                        surface.width = width.min(MAX_SURFACE_SIDE);
                    }
                    if height > 0 {
                        surface.height = height.min(MAX_SURFACE_SIDE);
                    }
                    surface.configured = surface.width > 0 && surface.height > 0;
                }
                if let Some(pending) = output.pending_launchers.take() {
                    let scale = pending.scale.max(1);
                    let whole = Rect::new(
                        0,
                        0,
                        pending.pixmap.width() as i32 / scale,
                        pending.pixmap.height() as i32 / scale,
                    );
                    let frame = Frame {
                        pixmap: &pending.pixmap,
                        scale: pending.scale,
                        damage: vec![whole],
                        input: pending.input.clone(),
                        visible: pending.input.clone(),
                        grab: false,
                    };
                    state.present_target(qh, Target::Launchers(data.0), &frame);
                }
            }
            zwlr_layer_surface_v1::Event::Closed => {
                state.events.push(Event::Quit);
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        {
            if caps.contains(wl_seat::Capability::Pointer) && state.pointer.is_none() {
                state.pointer = Some(seat.get_pointer(qh, ()));
            }
            if caps.contains(wl_seat::Capability::Keyboard) && state.keyboard.is_none() {
                state.keyboard = Some(seat.get_keyboard(qh, ()));
            }
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                serial,
                surface,
                surface_x,
                surface_y,
            } => {
                state.pointer_surface = state.surface_id(&surface);
                state.pointer_pos = (surface_x, surface_y);
                state.pointer_serial = serial;
                state.update_cursor();
                if let Some(id) = state.pointer_surface {
                    state.events.push(Event::PointerMotion {
                        surface: id,
                        x: surface_x,
                        y: surface_y,
                    });
                }
            }
            wl_pointer::Event::Leave { surface, .. } => {
                if let Some(id) = state.surface_id(&surface) {
                    state.events.push(Event::PointerLeave { surface: id });
                }
                state.pointer_surface = None;
            }
            wl_pointer::Event::Motion {
                surface_x, surface_y, ..
            } => {
                state.pointer_pos = (surface_x, surface_y);
                if let Some(id) = state.pointer_surface {
                    state.events.push(Event::PointerMotion {
                        surface: id,
                        x: surface_x,
                        y: surface_y,
                    });
                }
            }
            wl_pointer::Event::Button {
                serial,
                time,
                button,
                state: button_state,
            } => {
                state.button_serial = serial;
                let Some(id) = state.pointer_surface else {
                    return;
                };
                let button = match button {
                    BTN_LEFT => Button::Left,
                    BTN_RIGHT => Button::Right,
                    BTN_MIDDLE => Button::Middle,
                    _ => return,
                };
                let pressed = matches!(button_state, WEnum::Value(wl_pointer::ButtonState::Pressed));
                let (x, y) = state.pointer_pos;
                state.events.push(Event::PointerButton {
                    surface: id,
                    x,
                    y,
                    button,
                    pressed,
                    time,
                    modifiers: state.modifiers,
                });
            }
            wl_pointer::Event::Axis {
                axis: WEnum::Value(wl_pointer::Axis::VerticalScroll),
                value,
                ..
            } => {
                if let Some(id) = state.pointer_surface {
                    let (x, y) = state.pointer_pos;
                    state.events.push(Event::PointerAxis {
                        surface: id,
                        x,
                        y,
                        delta: value,
                    });
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Keymap { format, fd, size } => {
                if format == WEnum::Value(wl_keyboard::KeymapFormat::XkbV1)
                    && let Ok(map) = unsafe { memmap2::MmapOptions::new().len(size as usize).map(&fd) }
                {
                    let text = String::from_utf8_lossy(&map);
                    state.keymap = Keymap::parse(text.trim_end_matches('\0'));
                }
            }
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                ..
            } => {
                let held = mods_depressed | mods_latched;
                state.modifiers = Modifiers {
                    shift: held & 1 != 0,
                    ctrl: held & 4 != 0,
                    alt: held & 8 != 0,
                };
                state.caps_lock = mods_locked & 2 != 0;
            }
            wl_keyboard::Event::Key {
                key,
                state: key_state,
                ..
            } => {
                if key_state == WEnum::Value(wl_keyboard::KeyState::Pressed)
                    && let Some(surface) = state.keyboard_surface
                    && let Some(k) = state
                        .keymap
                        .translate(key + 8, state.modifiers.shift, state.caps_lock)
                {
                    state.events.push(Event::Key {
                        surface,
                        key: k,
                        modifiers: state.modifiers,
                    });
                }
            }
            wl_keyboard::Event::Enter { surface, .. } => {
                state.keyboard_surface = state.surface_id(&surface);
            }
            wl_keyboard::Event::Leave { surface, .. } => {
                if let Some(id) = state.surface_id(&surface) {
                    state.events.push(Event::KeyboardLeave { surface: id });
                }
                state.keyboard_surface = None;
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, Arc<AtomicBool>> for State {
    fn event(
        _: &mut Self,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        busy: &Arc<AtomicBool>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            busy.store(false, Ordering::Release);
        }
    }
}

impl Dispatch<zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1,
        event: zwlr_foreign_toplevel_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_foreign_toplevel_manager_v1::Event::Toplevel { toplevel } = event {
            let id = state.next_toplevel_id;
            state.next_toplevel_id += 1;
            state
                .handles
                .insert(toplevel.id(), Handle { id, handle: toplevel });
            state.events.push(Event::ToplevelAdded { id });
        }
    }

    event_created_child!(State, zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1, [
        zwlr_foreign_toplevel_manager_v1::EVT_TOPLEVEL_OPCODE => (zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        handle: &zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1,
        event: zwlr_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(id) = state.handles.get(&handle.id()).map(|h| h.id) else {
            return;
        };
        match event {
            zwlr_foreign_toplevel_handle_v1::Event::Title { title } => {
                state.events.push(Event::ToplevelTitle { id, title });
            }
            zwlr_foreign_toplevel_handle_v1::Event::AppId { app_id } => {
                state.events.push(Event::ToplevelAppId { id, app_id });
            }
            zwlr_foreign_toplevel_handle_v1::Event::State { state: flags } => {
                let mut toplevel_state = ToplevelState::default();
                for chunk in flags.as_chunks::<4>().0 {
                    let value = u32::from_ne_bytes(*chunk);
                    match value {
                        0 => toplevel_state.maximized = true,
                        1 => toplevel_state.minimized = true,
                        2 => toplevel_state.active = true,
                        3 => toplevel_state.fullscreen = true,
                        _ => {}
                    }
                }
                state.events.push(Event::ToplevelState {
                    id,
                    state: toplevel_state,
                });
            }
            zwlr_foreign_toplevel_handle_v1::Event::Closed => {
                if let Some(h) = state.handles.remove(&handle.id()) {
                    h.handle.destroy();
                }
                state.events.push(Event::ToplevelClosed { id });
            }
            _ => {}
        }
    }
}

delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_region::WlRegion);
delegate_noop!(State: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);
delegate_noop!(State: ignore zxdg_decoration_manager_v1::ZxdgDecorationManagerV1);
delegate_noop!(State: ignore xdg_toplevel_icon_manager_v1::XdgToplevelIconManagerV1);
delegate_noop!(State: ignore xdg_toplevel_icon_v1::XdgToplevelIconV1);
delegate_noop!(State: ignore zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1);
delegate_noop!(State: ignore wl_data_device_manager::WlDataDeviceManager);
