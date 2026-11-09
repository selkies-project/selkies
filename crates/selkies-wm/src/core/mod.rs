/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The window manager's engine: one window model and one set of decisions for both backends.
//!
//! The engine is a [`WindowPolicy`]: a backend hands it the compositor's events (a window added,
//! mapped, resized, or gone, a request, the pointer, a key, a tick) and applies the commands it
//! answers with (a configure, the stacking order, the focus, a frame, an overlay, an effect). It
//! holds every managed window with its geometry, states, and frame, the stacking order in
//! openbox's layers (desktop, below, normal, above, dock, and a focused fullscreen window over
//! all), the focus history Alt-Tab cycles and a closed window falls back through, the drag in
//! progress, the open menu, and the running effects. Geometry is in the compositor's logical
//! layout coordinates throughout; what differs per backend is only how a command is carried out.
//!
//! Decisions follow openbox where the images' configuration assumes it: a new window is focused
//! unless it declines focus or a rule says no, placed on the output under the pointer in the free
//! spot overlapping the least, framed unless it draws its own frame or asks for none; a click
//! focuses and raises; a modal dialog stays above its parent and takes the focus its parent is
//! given; a closed window's focus goes to the most recently focused one still shown; the bindings
//! are the ones `rc.xml` names over openbox's defaults. Every frame, menu, and on-screen display
//! is drawn through [`crate::frame`] and [`crate::menu`] from the desktop shell's own theme, so a
//! session looks the same whichever backend runs it.

pub mod effects;
pub mod grab;
pub mod pipes;
pub mod place;

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use pixelflux::wm::{
    Command, Event, Extents, Frame, KeyEvent, Modifiers, OutputId, OutputInfo, PointerEvent, PointerKind,
    Rect, Request, ResizeEdge, Size, States, Transform, Under, Verdict, WindowId, WindowInfo, WindowPolicy,
    WindowType,
};

use self::effects::{EffectKind, Effects};
use self::grab::{Grab, GrabKind};
use self::pipes::PipeJob;
use crate::config::{
    Action, Backend, Config, Context, Direction, KeyBind, MenuItem, Menus, MouseEvent, PlacementMonitor,
    RuleLayer, lower_keysym,
};
use crate::frame::{self, Appearance, Renderer};
use crate::i18n::tr;
use crate::menu::{self, Entry, Level, Target};
use crate::theme::ThemeWatch;
use selkies_draw::apps::{self, Apps};

/// What starts the programs the configuration names: `Execute` actions and pipe menus, run with
/// the display environment the backend knows.
pub trait Launcher: Send {
    /// Start a command line through the shell, detached.
    fn spawn(&mut self, command: &str);
    /// The environment a child is given, for the pipe menus' commands.
    fn env(&self) -> Vec<(String, String)>;
    /// The X server the compositor brought up for its X11 clients is on this display.
    fn set_x11_display(&mut self, _display: u32) {}
}

/// Linux event codes of the pointer buttons.
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

/// Overlay keys: the Alt-Tab list, and the menu levels from `MENU_KEY` up.
const CYCLE_KEY: u32 = 1;
const MENU_KEY: u32 = 100;

/// The menu id the engine fills with the installed applications by category, whatever a menu
/// file says it executes, so the images' `menu.xml` names it for openbox and labwc and gets the
/// same menu here without a pipe.
pub const APPLICATIONS_MENU: &str = "selkies-applications";
/// The ids of the applications menu's category submenus: the prefix and the category's name.
const APPLICATIONS_CATEGORY_PREFIX: &str = "selkies-applications/";

/// The translation key of a category's name, a static the catalogs carry.
fn category_key(name: &str) -> &'static str {
    match name {
        "Multimedia" => "Multimedia",
        "Development" => "Development",
        "Education" => "Education",
        "Games" => "Games",
        "Graphics" => "Graphics",
        "Internet" => "Internet",
        "Office" => "Office",
        "Science" => "Science",
        "Settings" => "Settings",
        "System" => "System",
        "Utilities" => "Utilities",
        _ => "Other",
    }
}

/// How far a cascade steps.
const CASCADE_STEP: i32 = frame::TITLE_HEIGHT;

/// The X keysyms the engine reads itself.
mod keys {
    pub const ESCAPE: u32 = 0xff1b;
    pub const RETURN: u32 = 0xff0d;
    pub const KP_ENTER: u32 = 0xff8d;
    pub const SPACE: u32 = 0x20;
    pub const LEFT: u32 = 0xff51;
    pub const UP: u32 = 0xff52;
    pub const RIGHT: u32 = 0xff53;
    pub const DOWN: u32 = 0xff54;
    pub const ALT_L: u32 = 0xffe9;
    pub const ALT_R: u32 = 0xffea;
    pub const SUPER_L: u32 = 0xffeb;
    pub const SUPER_R: u32 = 0xffec;
    pub const CONTROL_L: u32 = 0xffe3;
    pub const CONTROL_R: u32 = 0xffe4;
    pub const SHIFT_L: u32 = 0xffe1;
    pub const SHIFT_R: u32 = 0xffe2;
    pub const META_L: u32 = 0xffe7;
    pub const META_R: u32 = 0xffe8;
}

/// Whether a keysym is a modifier key.
fn is_modifier(keysym: u32) -> bool {
    matches!(
        keysym,
        keys::ALT_L
            | keys::ALT_R
            | keys::SUPER_L
            | keys::SUPER_R
            | keys::CONTROL_L
            | keys::CONTROL_R
            | keys::SHIFT_L
            | keys::SHIFT_R
            | keys::META_L
            | keys::META_R
    )
}

/// The modifiers that take part in matching a bind: the locks do not.
fn bind_modifiers(m: Modifiers) -> Modifiers {
    Modifiers {
        shift: m.shift,
        ctrl: m.ctrl,
        alt: m.alt,
        logo: m.logo,
        caps_lock: false,
        num_lock: false,
    }
}

/// The stacking layer of a window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Layer {
    Desktop,
    Below,
    Normal,
    Above,
    Dock,
    Fullscreen,
}

/// What a frame was last drawn from; a frame is drawn again only when this changes.
#[derive(Clone, Debug, PartialEq)]
struct FrameKey {
    title: String,
    active: bool,
    maximized: bool,
    shaded: bool,
    hover: Option<Context>,
    pressed: Option<Context>,
    width: i32,
    height: i32,
    scale: i32,
    look: u64,
    icon: bool,
}

/// One managed window.
struct Win {
    info: WindowInfo,
    mapped: bool,
    /// Whether the engine has chosen the window's place.
    placed: bool,
    /// The client area the engine wants, in layout coordinates.
    rect: Rect,
    /// The size the client last drew at.
    committed: Option<Size>,
    /// The edge the last resize drag held still, kept until the client's answer to that drag
    /// has arrived, so a size the client snaps to after the release keeps that edge in place.
    resize_anchor: Option<ResizeEdge>,
    /// The geometry before a maximize or fullscreen.
    saved: Option<Rect>,
    maximized: bool,
    fullscreen: bool,
    minimized: bool,
    shaded: bool,
    above: bool,
    below: bool,
    decorated: bool,
    attention: bool,
    skip_taskbar: bool,
    focusable: bool,
    /// The last configure sent, so an unchanged one is not sent again.
    sent: Option<(Rect, States)>,
    frame_key: Option<FrameKey>,
    output: OutputId,
    taskbar: Option<Rect>,
    hover: Option<Context>,
    pressed: Option<Context>,
    layer_rule: Option<RuleLayer>,
    /// Minimized by a show-desktop, to come back with the restore.
    by_show_desktop: bool,
}

impl Win {
    fn kind_layer(&self, focused: bool) -> Layer {
        if self.fullscreen && focused {
            return Layer::Fullscreen;
        }
        match self.info.window_type {
            WindowType::Desktop => Layer::Desktop,
            WindowType::Dock => Layer::Dock,
            WindowType::Notification => Layer::Above,
            _ => {
                if self.above || self.layer_rule == Some(RuleLayer::Above) {
                    Layer::Above
                } else if self.below || self.layer_rule == Some(RuleLayer::Below) {
                    Layer::Below
                } else {
                    Layer::Normal
                }
            }
        }
    }

    /// Whether the window is shown: mapped and not minimized.
    fn shown(&self) -> bool {
        self.mapped && !self.minimized
    }

    /// Whether the window takes part in the taskbar and Alt-Tab.
    fn listed(&self) -> bool {
        !self.skip_taskbar
            && matches!(
                self.info.window_type,
                WindowType::Normal | WindowType::Dialog | WindowType::Utility
            )
    }

    fn states(&self, focused: bool) -> States {
        States {
            activated: focused,
            maximized: self.maximized,
            fullscreen: self.fullscreen,
            resizing: false,
            minimized: self.minimized,
        }
    }
}

/// A button held down since its press, until it is released.
#[derive(Clone, Copy, Debug)]
struct Press {
    button: u32,
    context: Context,
    window: Option<WindowId>,
    at: (f64, f64),
    dragged: bool,
    consumed: bool,
}

/// The open menu: its levels, root first, and what it is about.
struct MenuState {
    levels: Vec<Level>,
    /// The window a window menu or a window list acts on.
    window: Option<WindowId>,
    output: OutputId,
    /// A button held since the press that opened the menu, so its release does not pick.
    opening_button: Option<u32>,
}

/// An Alt-Tab in progress.
struct Cycle {
    windows: Vec<WindowId>,
    index: usize,
    /// The modifiers of the bind that started it; their release applies the choice.
    modifiers: Modifiers,
}

/// The engine; see the module documentation.
pub struct Engine {
    config: Config,
    menus: Menus,
    backend: Backend,
    theme: ThemeWatch,
    /// Counts theme changes, so a frame drawn under an older look is drawn again.
    look_generation: u64,
    renderer: Renderer,
    launcher: Box<dyn Launcher>,
    outputs: Vec<OutputInfo>,
    windows: HashMap<WindowId, Win>,
    /// Every shown window, bottom first, within and across layers.
    stack: Vec<WindowId>,
    /// Focus history, most recent first.
    mru: Vec<WindowId>,
    focus: Option<WindowId>,
    pointer: (f64, f64),
    press: Option<Press>,
    last_click: Option<(u32, Context, Option<WindowId>, u32)>,
    grab: Option<Grab>,
    menu: Option<MenuState>,
    cycle: Option<Cycle>,
    effects: Effects,
    pipes: Vec<PipeJob>,
    /// The desktop entries the applications menu lists, read when it first opens and again
    /// once an applications directory changed or the configuration was reloaded.
    apps: Option<Apps>,
    /// Keycodes whose press was taken, so their release is taken too.
    consumed_keys: HashSet<u32>,
    /// A bind that runs on its key's release, armed by the press, disarmed by another key.
    release_bind: Option<KeyBind>,
    cursor: &'static str,
    showing_desktop: bool,
    unknown_logged: HashSet<String>,
    now: Instant,
    out: Vec<Command>,
    /// The next time the engine wants a tick for the theme watch.
    theme_poll: Instant,
}

impl Engine {
    /// An engine for a backend, its configuration and menus read from the files, its programs
    /// started through `launcher`.
    pub fn new(backend: Backend, launcher: Box<dyn Launcher>) -> Engine {
        let config = Config::load(backend);
        let menus = Menus::load(&config);
        let theme = ThemeWatch::new();
        crate::i18n::select(
            selkies_draw::style::desktop_values()
                .get("language")
                .map(String::as_str),
        );
        let renderer = Renderer::new(theme.icon_theme.as_deref());
        let sources: Vec<String> = config
            .sources
            .iter()
            .chain(menus.sources.iter())
            .map(|p| p.display().to_string())
            .collect();
        log::info!(
            target: "wm",
            "window manager on {}: {} keybinds, {} mousebinds, {} menus, configuration from {}",
            match backend {
                Backend::Wayland => "Wayland",
                Backend::X11 => "X11",
            },
            config.keybinds.len(),
            config.mousebinds.len(),
            menus.menus.len(),
            if sources.is_empty() { "the defaults".to_string() } else { sources.join(", ") }
        );
        if let Some(name) = &config.theme_name {
            log::debug!(target: "wm", "rc.xml names the {name} theme; the frames follow the desktop's theme");
        }
        let now = Instant::now();
        Engine {
            theme_poll: now + Duration::from_secs(1),
            config,
            menus,
            backend,
            theme,
            look_generation: 0,
            renderer,
            launcher,
            outputs: Vec::new(),
            windows: HashMap::new(),
            stack: Vec::new(),
            mru: Vec::new(),
            focus: None,
            pointer: (0.0, 0.0),
            press: None,
            last_click: None,
            grab: None,
            menu: None,
            cycle: None,
            effects: Effects::default(),
            pipes: Vec::new(),
            apps: None,
            consumed_keys: HashSet::new(),
            release_bind: None,
            cursor: "default",
            showing_desktop: false,
            unknown_logged: HashSet::new(),
            now,
            out: Vec::new(),
        }
    }

    /// The key bindings in force, for a backend that grabs them.
    pub fn keybinds(&self) -> &[KeyBind] {
        &self.config.keybinds
    }

    /// The pointer bindings in force, for a backend that grabs the ones acting inside clients.
    pub fn mousebinds(&self) -> &[crate::config::MouseBind] {
        &self.config.mousebinds
    }

    /// The look the frames are drawn with.
    /// Whether a move or resize grab is in progress.
    pub fn grabbing(&self) -> bool {
        self.grab.is_some()
    }

    pub fn look(&self) -> &crate::theme::Look {
        &self.theme.look
    }

    /// Read the configuration and the menus again, as the `Reconfigure` action does.
    pub fn reconfigure(&mut self) {
        let config = Config::load(self.backend);
        self.menus = Menus::load(&config);
        self.apps = None;
        self.config = config;
        self.close_menu();
        for id in self.windows.keys().copied().collect::<Vec<_>>() {
            self.apply_decoration_choice(id);
            self.render_frame(id);
        }
        self.emit_stack();
        log::info!(target: "wm", "configuration read again: {} keybinds, {} mousebinds, {} menus",
            self.config.keybinds.len(), self.config.mousebinds.len(), self.menus.menus.len());
    }

    // ---- outputs -------------------------------------------------------------------------

    fn output(&self, id: OutputId) -> Option<&OutputInfo> {
        self.outputs.iter().find(|o| o.id == id)
    }

    fn primary_output(&self) -> Option<&OutputInfo> {
        self.outputs
            .iter()
            .find(|o| o.primary)
            .or_else(|| self.outputs.first())
    }

    fn output_at(&self, x: f64, y: f64) -> Option<&OutputInfo> {
        self.outputs
            .iter()
            .find(|o| o.rect.contains_f64(x, y))
            .or_else(|| self.primary_output())
    }

    /// The output a window's frame mostly lies on, else the one it was placed on.
    fn output_of_rect(&self, rect: Rect) -> Option<&OutputInfo> {
        self.outputs
            .iter()
            .max_by_key(|o| o.rect.intersection(&rect).area())
            .filter(|o| o.rect.intersection(&rect).area() > 0)
    }

    fn workarea(&self, output: OutputId) -> Rect {
        self.output(output)
            .map(|o| o.workarea)
            .or_else(|| self.primary_output().map(|o| o.workarea))
            .unwrap_or(Rect::new(0, 0, 1024, 768))
    }

    fn output_rect(&self, output: OutputId) -> Rect {
        self.output(output)
            .map(|o| o.rect)
            .or_else(|| self.primary_output().map(|o| o.rect))
            .unwrap_or(Rect::new(0, 0, 1024, 768))
    }

    fn scale_of(&self, output: OutputId) -> i32 {
        self.output(output)
            .map(|o| o.scale.ceil().max(1.0) as i32)
            .unwrap_or(1)
    }

    fn outputs_changed(&mut self, outputs: Vec<OutputInfo>) {
        let gone: Vec<OutputId> = self
            .outputs
            .iter()
            .map(|o| o.id)
            .filter(|id| !outputs.iter().any(|o| o.id == *id))
            .collect();
        self.outputs = outputs;
        let primary = self.primary_output().map(|o| o.id).unwrap_or(0);
        let ids: Vec<WindowId> = self.windows.keys().copied().collect();
        for id in ids {
            let Some(win) = self.windows.get(&id) else {
                continue;
            };
            let mut output = win.output;
            if gone.contains(&output) || self.output(output).is_none() {
                output = primary;
            }
            let (maximized, fullscreen, placed, rect) = (win.maximized, win.fullscreen, win.placed, win.rect);
            let exact = places_itself(win.info.window_type);
            if let Some(win) = self.windows.get_mut(&id) {
                win.output = output;
            }
            if fullscreen {
                let area = self.output_rect(output);
                self.set_rect(id, area);
            } else if maximized {
                self.fit_maximized(id);
            } else if placed && !exact {
                let extents = self.extents_of(id);
                let frame = rect.grown(extents);
                let area = self.workarea(output);
                let clamped = place::clamp(frame, area);
                if clamped != frame || gone.contains(&win_output(&self.windows, id)) {
                    let client = grab::client_of(clamped, extents);
                    self.set_rect(id, client);
                }
            }
            self.render_frame(id);
        }
        self.close_menu();
        self.emit_stack();
    }

    // ---- windows --------------------------------------------------------------------------

    fn win(&self, id: WindowId) -> Option<&Win> {
        self.windows.get(&id)
    }

    fn extents_of(&self, id: WindowId) -> Extents {
        let Some(win) = self.win(id) else {
            return Extents::default();
        };
        if !win.decorated || win.fullscreen {
            return Extents::default();
        }
        frame::extents(&self.theme.look.style, win.maximized, win.shaded)
    }

    fn frame_rect(&self, id: WindowId) -> Rect {
        let Some(win) = self.win(id) else {
            return Rect::default();
        };
        win.rect.grown(self.extents_of(id))
    }

    /// Whether a client area is drawn by the server: the style says frames are themed, the
    /// window is a kind that takes one, a rule or the client did not decline.
    fn apply_decoration_choice(&mut self, id: WindowId) {
        let Some(win) = self.windows.get(&id) else {
            return;
        };
        let by_type = matches!(
            win.info.window_type,
            WindowType::Normal | WindowType::Dialog | WindowType::Utility | WindowType::Toolbar
        );
        let by_client = match win.info.decoration {
            pixelflux::wm::Decoration::Server => true,
            pixelflux::wm::Decoration::Unspecified => self.config.decorate_by_default,
            pixelflux::wm::Decoration::Client | pixelflux::wm::Decoration::None => false,
        };
        let rule = self.rule_for(&win.info).and_then(|r| r.decor);
        let decorated = self.theme.look.style.decorate && by_type && rule.unwrap_or(by_client);
        if let Some(win) = self.windows.get_mut(&id) {
            win.decorated = decorated;
        }
    }

    fn rule_for(&self, info: &WindowInfo) -> Option<&crate::config::Rule> {
        let type_name = match info.window_type {
            WindowType::Normal => "normal",
            WindowType::Dialog => "dialog",
            WindowType::Utility => "utility",
            WindowType::Toolbar => "toolbar",
            WindowType::Menu => "menu",
            WindowType::Splash => "splash",
            WindowType::Dock => "dock",
            WindowType::Desktop => "desktop",
            WindowType::Notification => "notification",
            _ => "normal",
        };
        let instance = if info.instance.is_empty() {
            info.app_id.as_str()
        } else {
            info.instance.as_str()
        };
        self.config
            .rules
            .iter()
            .find(|r| r.matches(instance, &info.app_id, &info.title, type_name))
    }

    fn added(&mut self, info: WindowInfo) {
        let id = info.id;
        let output = self.output(info.output).map(|o| o.id).unwrap_or_else(|| {
            self.output_at(self.pointer.0, self.pointer.1)
                .map(|o| o.id)
                .unwrap_or(0)
        });
        let rule = self.rule_for(&info).cloned();
        let placement_output = match self.config.placement_monitor {
            PlacementMonitor::Mouse => self
                .output_at(self.pointer.0, self.pointer.1)
                .map(|o| o.id)
                .unwrap_or(output),
            PlacementMonitor::Active => self
                .focus
                .and_then(|f| self.win(f))
                .map(|w| w.output)
                .unwrap_or(output),
            PlacementMonitor::Primary => self.primary_output().map(|o| o.id).unwrap_or(output),
        };
        let output = rule
            .as_ref()
            .and_then(|r| r.monitor)
            .and_then(|n| self.outputs.get(n.saturating_sub(1) as usize))
            .map(|o| o.id)
            .unwrap_or(placement_output);
        let requested = info.requested;
        let mut win = Win {
            rect: requested.unwrap_or(Rect::new(0, 0, 0, 0)),
            mapped: false,
            placed: false,
            committed: None,
            resize_anchor: None,
            saved: None,
            maximized: info.wants_maximized || rule.as_ref().and_then(|r| r.maximized).unwrap_or(false),
            fullscreen: info.wants_fullscreen || rule.as_ref().and_then(|r| r.fullscreen).unwrap_or(false),
            minimized: false,
            shaded: rule.as_ref().and_then(|r| r.shade).unwrap_or(false),
            above: false,
            below: false,
            decorated: true,
            attention: false,
            skip_taskbar: info.skip_taskbar || rule.as_ref().and_then(|r| r.skip_taskbar).unwrap_or(false),
            focusable: info.accepts_focus
                && !matches!(
                    info.window_type,
                    WindowType::Dock
                        | WindowType::Desktop
                        | WindowType::Notification
                        | WindowType::Tooltip
                        | WindowType::Splash
                        | WindowType::Dnd
                ),
            sent: None,
            frame_key: None,
            output,
            taskbar: None,
            hover: None,
            pressed: None,
            layer_rule: rule.as_ref().and_then(|r| r.layer),
            by_show_desktop: false,
            info,
        };
        let wants_minimized =
            win.info.wants_minimized || rule.as_ref().and_then(|r| r.iconic).unwrap_or(false);
        win.minimized = wants_minimized;
        self.windows.insert(id, win);
        self.apply_decoration_choice(id);
        if let Some((w, h)) = rule.as_ref().and_then(|r| r.size)
            && let Some(win) = self.windows.get_mut(&id)
        {
            win.rect.w = w;
            win.rect.h = h;
        }
        let has_size = self.win(id).is_some_and(|w| w.rect.w > 0 && w.rect.h > 0);
        if has_size {
            self.place(id);
        }
        self.configure(id);
    }

    /// Choose a window's place from its size: a rule's position, the client's request where it
    /// made one, a dialog over its parent, else the placement policy on its output's workarea.
    fn place(&mut self, id: WindowId) {
        let Some(win) = self.win(id) else {
            return;
        };
        if win.placed {
            return;
        }
        let output = win.output;
        let extents = self.extents_of(id);
        let size = win.rect;
        let frame_size = (
            size.w + extents.left + extents.right,
            size.h + extents.top + extents.bottom,
        );
        let area = self.workarea(output);
        let rule = self.rule_for(&win.info).cloned();
        let parent_rect = win
            .info
            .parent
            .and_then(|p| self.win(p))
            .filter(|p| p.shown())
            .map(|p| p.rect);
        let frame = if let Some((x, y)) = rule.as_ref().and_then(|r| r.position) {
            Rect::new(
                x.resolve(area.x, area.w, frame_size.0),
                y.resolve(area.y, area.h, frame_size.1),
                frame_size.0,
                frame_size.1,
            )
        } else if let Some(requested) = win.info.requested.filter(|_| win.info.positioned) {
            // The client's own position, as openbox keeps a program- or user-specified one;
            // a window that places itself (a dock, a desktop, a notification) is not even kept
            // on screen, since where it lies is its own business.
            let frame = Rect::new(
                requested.x - extents.left,
                requested.y - extents.top,
                frame_size.0,
                frame_size.1,
            );
            if places_itself(win.info.window_type) {
                frame
            } else {
                place::clamp(frame, area)
            }
        } else if let Some(parent) = parent_rect
            && matches!(win.info.window_type, WindowType::Dialog | WindowType::Normal)
            && win.info.parent.is_some()
        {
            place::clamp(
                Rect::new(
                    parent.x + (parent.w - frame_size.0) / 2,
                    parent.y + (parent.h - frame_size.1) / 2,
                    frame_size.0,
                    frame_size.1,
                ),
                area,
            )
        } else if matches!(win.info.window_type, WindowType::Dock | WindowType::Desktop) {
            let requested = win.info.requested.unwrap_or(size);
            Rect::new(requested.x, requested.y, frame_size.0, frame_size.1)
        } else {
            let others: Vec<Rect> = self
                .stack
                .iter()
                .filter(|w| **w != id)
                .filter_map(|w| self.win(*w))
                .filter(|w| w.shown() && w.output == output && w.listed())
                .map(|w| {
                    w.rect
                        .grown(frame::extents(&self.theme.look.style, w.maximized, w.shaded))
                })
                .collect();
            let policy = if self.config.placement_center
                && others.is_empty()
                && self.config.placement == crate::config::PlacementPolicy::Smart
            {
                crate::config::PlacementPolicy::Center
            } else {
                self.config.placement
            };
            place::place(
                policy,
                frame_size,
                area,
                &others,
                (self.pointer.0 as i32, self.pointer.1 as i32),
                CASCADE_STEP,
            )
        };
        let client = grab::client_of(frame, extents);
        if let Some(win) = self.windows.get_mut(&id) {
            win.rect = client;
            win.placed = true;
        }
    }

    fn mapped(&mut self, id: WindowId, size: Size) {
        let Some(win) = self.windows.get_mut(&id) else {
            return;
        };
        let first = !win.mapped;
        win.mapped = true;
        win.committed = Some(size);
        if !win.fullscreen && !win.maximized && size.w > 0 && size.h > 0 {
            win.rect.w = size.w;
            win.rect.h = size.h;
        }
        let output = win.output;
        let (minimized, focusable, fullscreen, maximized) =
            (win.minimized, win.focusable, win.fullscreen, win.maximized);
        self.place(id);
        if fullscreen {
            let area = self.output_rect(output);
            self.set_rect(id, area);
        } else if maximized {
            self.fit_maximized(id);
        }
        if !self.stack.contains(&id) && !minimized {
            self.stack.push(id);
        }
        let focus_new = self.config.focus_new
            && self.rule_for(&self.windows[&id].info).and_then(|r| r.focus) != Some(false);
        if first && !minimized && focusable && focus_new && self.menu.is_none() {
            self.set_focus(Some(id));
        } else if first && !minimized {
            self.raise(id);
        }
        if first && !minimized {
            let rect = self.windows[&id].rect;
            let shrunk = Rect::new(
                rect.x + rect.w / 40,
                rect.y + rect.h / 40,
                rect.w - rect.w / 20,
                rect.h - rect.h / 20,
            );
            self.effects.start(id, EffectKind::Map, shrunk, rect, self.now);
            self.out.push(Command::WakeAt(self.now));
        }
        self.configure(id);
        self.render_frame(id);
        self.emit_stack();
    }

    fn resized(&mut self, id: WindowId, size: Size) {
        let Some(win) = self.windows.get_mut(&id) else {
            return;
        };
        win.committed = Some(size);
        if let Some(grab) = self.grab.as_ref().filter(|g| g.window == id) {
            if size.w > 0 && size.h > 0 && (win.rect.w != size.w || win.rect.h != size.h) {
                let wanted = win.rect;
                let mut rect = wanted;
                if let GrabKind::Resize(edge) = grab.kind {
                    if edge.left() {
                        rect.x = wanted.right() - size.w;
                    }
                    if edge.top() {
                        rect.y = wanted.bottom() - size.h;
                    }
                }
                rect.w = size.w;
                rect.h = size.h;
                win.rect = rect;
                self.configure(id);
                self.render_frame(id);
            }
            return;
        }
        if size.w > 0 && size.h > 0 && (win.rect.w != size.w || win.rect.h != size.h) {
            if win.maximized || win.fullscreen {
                return;
            }
            let wanted = win.rect;
            if let Some(edge) = win.resize_anchor.take() {
                if edge.left() {
                    win.rect.x = wanted.right() - size.w;
                }
                if edge.top() {
                    win.rect.y = wanted.bottom() - size.h;
                }
            }
            win.rect.w = size.w;
            win.rect.h = size.h;
            let extents = self.extents_of(id);
            let area = self.workarea(self.windows[&id].output);
            let frame = place::clamp(self.windows[&id].rect.grown(extents), area);
            let client = grab::client_of(frame, extents);
            if let Some(win) = self.windows.get_mut(&id) {
                win.rect = client;
                // A size the client chose itself needs no configure back, but a position that
                // followed it (an anchored edge, a clamp) does.
                if client.x == wanted.x && client.y == wanted.y {
                    win.sent = win.sent.map(|(_, s)| (client, s));
                }
            }
            self.configure(id);
            self.render_frame(id);
        }
    }

    fn unmapped(&mut self, id: WindowId) {
        let Some(win) = self.windows.get_mut(&id) else {
            return;
        };
        win.mapped = false;
        win.frame_key = None;
        win.sent = None;
        self.effects.cancel(id);
        self.stack.retain(|w| *w != id);
        self.out.push(Command::Frame { id, frame: None });
        if self.focus == Some(id) {
            self.focus_fallback();
        }
        if self.grab.as_ref().is_some_and(|g| g.window == id) {
            self.end_grab();
        }
        self.emit_stack();
    }

    fn removed(&mut self, id: WindowId) {
        self.unmapped(id);
        self.windows.remove(&id);
        self.mru.retain(|w| *w != id);
        if self.menu.as_ref().is_some_and(|m| m.window == Some(id)) {
            self.close_menu();
        }
        if let Some(cycle) = self.cycle.as_mut() {
            cycle.windows.retain(|w| *w != id);
            if cycle.windows.is_empty() {
                self.cycle = None;
                self.out.push(Command::ClearOverlay(CYCLE_KEY));
            } else {
                cycle.index = cycle.index.min(cycle.windows.len() - 1);
                self.render_cycle();
            }
        }
    }

    fn changed(&mut self, info: WindowInfo) {
        let id = info.id;
        let Some(win) = self.windows.get_mut(&id) else {
            return;
        };
        let was = std::mem::replace(&mut win.info, info);
        let (decoration_changed, parent_changed) = {
            let win = &self.windows[&id];
            (
                was.decoration != win.info.decoration || was.window_type != win.info.window_type,
                was.parent != win.info.parent,
            )
        };
        if decoration_changed {
            self.apply_decoration_choice(id);
            self.configure(id);
        }
        if parent_changed {
            self.emit_stack();
        }
        self.render_frame(id);
    }

    // ---- geometry and states ---------------------------------------------------------------

    /// The client area the engine wants; the configure follows when it changed.
    fn set_rect(&mut self, id: WindowId, rect: Rect) {
        if let Some(win) = self.windows.get_mut(&id) {
            win.rect = rect;
            win.placed = true;
        }
        self.configure(id);
    }

    /// Send the window's configure when its geometry or states differ from the last one sent.
    fn configure(&mut self, id: WindowId) {
        let focused = self.focus == Some(id);
        let Some(win) = self.windows.get_mut(&id) else {
            return;
        };
        let states = win.states(focused);
        let rect = win.rect;
        if win.sent == Some((rect, states)) {
            return;
        }
        win.sent = Some((rect, states));
        self.out.push(Command::Configure { id, rect, states });
    }

    fn fit_maximized(&mut self, id: WindowId) {
        let Some(win) = self.win(id) else {
            return;
        };
        let area = self.workarea(win.output);
        let extents = if win.decorated {
            frame::extents(&self.theme.look.style, true, false)
        } else {
            Extents::default()
        };
        let client = grab::client_of(area, extents);
        self.set_rect(id, client);
    }

    fn set_maximized(&mut self, id: WindowId, on: bool) {
        let Some(win) = self.windows.get_mut(&id) else {
            return;
        };
        if win.maximized == on || !win.mapped {
            return;
        }
        if win.fullscreen {
            win.maximized = on;
            return;
        }
        if on {
            win.saved = Some(win.rect);
            win.maximized = true;
            win.shaded = false;
            self.fit_maximized(id);
        } else {
            win.maximized = false;
            let restored = win.saved.take().unwrap_or(win.rect);
            let extents = self.extents_of(id);
            let area = self.workarea(self.windows[&id].output);
            let frame = place::clamp(restored.grown(extents), area);
            self.set_rect(id, grab::client_of(frame, extents));
        }
        self.render_frame(id);
    }

    fn set_fullscreen(&mut self, id: WindowId, on: bool, output: Option<OutputId>) {
        let Some(win) = self.windows.get_mut(&id) else {
            return;
        };
        if let Some(output) = output.filter(|o| self.outputs.iter().any(|x| x.id == *o)) {
            win.output = output;
        }
        let (already, win_output, maximized, rect) = (win.fullscreen, win.output, win.maximized, win.rect);
        if already == on {
            if on {
                let area = self.output_rect(win_output);
                self.set_rect(id, area);
            }
            return;
        }
        if on {
            if let Some(win) = self.windows.get_mut(&id) {
                if !maximized {
                    win.saved = Some(rect);
                }
                win.fullscreen = true;
                win.shaded = false;
                win.frame_key = None;
            }
            let area = self.output_rect(win_output);
            self.set_rect(id, area);
            self.out.push(Command::Frame { id, frame: None });
        } else {
            if let Some(win) = self.windows.get_mut(&id) {
                win.fullscreen = false;
            }
            if maximized {
                self.fit_maximized(id);
            } else {
                let restored = self
                    .windows
                    .get_mut(&id)
                    .and_then(|w| w.saved.take())
                    .unwrap_or(rect);
                let extents = self.extents_of(id);
                let area = self.workarea(win_output);
                let frame = place::clamp(restored.grown(extents), area);
                self.set_rect(id, grab::client_of(frame, extents));
            }
            self.render_frame(id);
        }
        self.emit_stack();
    }

    fn set_minimized(&mut self, id: WindowId, on: bool) {
        let Some(win) = self.win(id) else {
            return;
        };
        if win.minimized == on || !win.mapped {
            if on && !win.mapped {
                if let Some(win) = self.windows.get_mut(&id) {
                    win.minimized = true;
                }
                self.configure(id);
            }
            return;
        }
        if on {
            let rect = win.rect;
            let target = Effects::minimize_target(win.taskbar, self.output_rect(win.output));
            self.effects
                .start(id, EffectKind::Minimize, rect, target, self.now);
            self.out.push(Command::WakeAt(self.now));
            if self.focus == Some(id) {
                self.focus_fallback_excluding(id);
            }
        } else {
            if let Some(win) = self.windows.get_mut(&id) {
                win.minimized = false;
                win.by_show_desktop = false;
            }
            if !self.stack.contains(&id) {
                self.stack.push(id);
            }
            self.configure(id);
            let win = &self.windows[&id];
            let rect = win.rect;
            let from = Effects::minimize_target(win.taskbar, self.output_rect(win.output));
            self.effects.start(id, EffectKind::Restore, from, rect, self.now);
            self.out.push(Command::WakeAt(self.now));
            self.render_frame(id);
            self.emit_stack();
        }
    }

    /// Hide a window whose minimize effect ended.
    fn finish_minimize(&mut self, id: WindowId) {
        let Some(win) = self.windows.get_mut(&id) else {
            return;
        };
        win.minimized = true;
        win.hover = None;
        win.pressed = None;
        self.stack.retain(|w| *w != id);
        self.out.push(Command::Frame { id, frame: None });
        if let Some(win) = self.windows.get_mut(&id) {
            win.frame_key = None;
        }
        self.configure(id);
        self.emit_stack();
    }

    fn set_shaded(&mut self, id: WindowId, on: bool) {
        let Some(win) = self.windows.get_mut(&id) else {
            return;
        };
        if win.shaded == on || !win.decorated || win.fullscreen {
            return;
        }
        win.shaded = on;
        let rect = win.rect;
        self.out.push(Command::Effect {
            id,
            transform: on.then_some(Transform {
                kind: EffectKind::Shade,
                rect: Rect::new(rect.x, rect.y, rect.w, 0),
                alpha: 0.0,
            }),
        });
        self.render_frame(id);
    }

    fn set_decorated(&mut self, id: WindowId, on: bool) {
        let Some(win) = self.windows.get_mut(&id) else {
            return;
        };
        if win.decorated == on {
            return;
        }
        win.decorated = on;
        if !on {
            win.shaded = false;
            self.out.push(Command::Frame { id, frame: None });
            if let Some(win) = self.windows.get_mut(&id) {
                win.frame_key = None;
            }
        }
        if self.windows[&id].maximized {
            self.fit_maximized(id);
        }
        self.render_frame(id);
    }

    // ---- stacking and focus ----------------------------------------------------------------

    /// Raise a window within its layer, its transient children over it.
    fn raise(&mut self, id: WindowId) {
        if !self.stack.contains(&id) {
            return;
        }
        self.stack.retain(|w| *w != id);
        self.stack.push(id);
        let children: Vec<WindowId> = self
            .stack
            .iter()
            .copied()
            .filter(|w| self.win(*w).is_some_and(|c| c.info.parent == Some(id)))
            .collect();
        for child in children {
            self.stack.retain(|w| *w != child);
            self.stack.push(child);
        }
        if let Some(win) = self.windows.get_mut(&id) {
            win.attention = false;
        }
        self.emit_stack();
    }

    fn lower(&mut self, id: WindowId) {
        if !self.stack.contains(&id) {
            return;
        }
        self.stack.retain(|w| *w != id);
        self.stack.insert(0, id);
        self.emit_stack();
    }

    /// The stacking order in layers, bottom first.
    fn ordered_stack(&self) -> Vec<WindowId> {
        let mut ordered: Vec<(Layer, usize, WindowId)> = self
            .stack
            .iter()
            .enumerate()
            .filter_map(|(i, id)| {
                self.win(*id)
                    .filter(|w| w.shown())
                    .map(|w| (w.kind_layer(self.focus == Some(*id)), i, *id))
            })
            .collect();
        ordered.sort_by_key(|(layer, i, _)| (*layer, *i));
        ordered.into_iter().map(|(_, _, id)| id).collect()
    }

    fn emit_stack(&mut self) {
        let order = self.ordered_stack();
        if let Some(Command::Stack(last)) = self.out.iter().rev().find(|c| matches!(c, Command::Stack(_)))
            && *last == order
        {
            return;
        }
        self.out.push(Command::Stack(order));
    }

    /// The window to focus for `id`: its modal child while it has one shown.
    fn focus_target(&self, id: WindowId) -> WindowId {
        let mut current = id;
        for _ in 0..8 {
            let modal = self.stack.iter().rev().copied().find(|w| {
                self.win(*w)
                    .is_some_and(|c| c.info.parent == Some(current) && c.info.modal && c.shown())
            });
            match modal {
                Some(m) => current = m,
                None => break,
            }
        }
        current
    }

    fn set_focus(&mut self, id: Option<WindowId>) {
        let id = id
            .map(|i| self.focus_target(i))
            .filter(|i| self.win(*i).is_some_and(|w| w.shown()));
        let id = match id {
            Some(i) if !self.windows[&i].focusable => {
                self.raise(i);
                return;
            }
            other => other,
        };
        if self.focus == id {
            if let Some(i) = id {
                self.raise(i);
            }
            return;
        }
        let previous = self.focus;
        self.focus = id;
        self.out.push(Command::Focus(id));
        if let Some(prev) = previous {
            self.configure(prev);
            self.render_frame(prev);
        }
        if let Some(i) = id {
            self.mru.retain(|w| *w != i);
            self.mru.insert(0, i);
            if let Some(win) = self.windows.get_mut(&i) {
                win.attention = false;
            }
            self.configure(i);
            self.raise(i);
            self.render_frame(i);
        }
        self.emit_stack();
    }

    fn focus_fallback(&mut self) {
        let next = self
            .mru
            .iter()
            .copied()
            .find(|w| self.win(*w).is_some_and(|c| c.shown() && c.focusable));
        self.focus = None;
        self.set_focus(next);
        if next.is_none() {
            self.out.push(Command::Focus(None));
        }
    }

    fn focus_fallback_excluding(&mut self, excluded: WindowId) {
        let next = self
            .mru
            .iter()
            .copied()
            .find(|w| *w != excluded && self.win(*w).is_some_and(|c| c.shown() && c.focusable));
        self.focus = None;
        self.set_focus(next);
        if next.is_none() {
            self.out.push(Command::Focus(None));
        }
        if let Some(win) = self.windows.get_mut(&excluded) {
            win.sent = None;
        }
        self.configure(excluded);
        self.render_frame(excluded);
    }

    // ---- frames ------------------------------------------------------------------------------

    fn render_frame(&mut self, id: WindowId) {
        let Some(win) = self.win(id) else {
            return;
        };
        if !win.shown() || !win.decorated || win.fullscreen {
            if win.frame_key.is_some() {
                self.out.push(Command::Frame { id, frame: None });
                if let Some(win) = self.windows.get_mut(&id) {
                    win.frame_key = None;
                }
            }
            return;
        }
        let scale = self.scale_of(win.output);
        let active = self.focus == Some(id);
        let key = FrameKey {
            title: win.info.title.clone(),
            active,
            maximized: win.maximized,
            shaded: win.shaded,
            hover: win.hover,
            pressed: win.pressed,
            width: win.rect.w,
            height: win.rect.h,
            scale,
            look: self.look_generation,
            icon: false,
        };
        if win.frame_key.as_ref() == Some(&key) {
            return;
        }
        let client = Size {
            w: win.rect.w,
            h: win.rect.h,
        };
        let (title, app_id, maximized, shaded, hover, pressed) = (
            win.info.title.clone(),
            win.info.app_id.clone(),
            win.maximized,
            win.shaded,
            win.hover,
            win.pressed,
        );
        let appearance = Appearance {
            title: &title,
            icon: None,
            app_id: &app_id,
            active,
            maximized,
            shaded,
            hover,
            pressed,
            scale,
        };
        let look = self.theme.look;
        let frame: Frame = self.renderer.render(&look, client, &appearance);
        if let Some(win) = self.windows.get_mut(&id) {
            win.frame_key = Some(key);
        }
        self.out.push(Command::Frame {
            id,
            frame: Some(frame),
        });
    }

    fn set_cursor(&mut self, name: &'static str) {
        if self.cursor != name {
            self.cursor = name;
            self.out.push(Command::Cursor(name));
        }
    }

    // ---- requests ----------------------------------------------------------------------------

    fn request(&mut self, id: WindowId, request: Request) {
        if self.win(id).is_none() {
            return;
        }
        match request {
            Request::Maximize => self.set_maximized(id, true),
            Request::Unmaximize => self.set_maximized(id, false),
            Request::Fullscreen(output) => self.set_fullscreen(id, true, output),
            Request::Unfullscreen => self.set_fullscreen(id, false, None),
            Request::Minimize => self.set_minimized(id, true),
            Request::Unminimize => self.set_minimized(id, false),
            Request::Move => {
                let button = self.press.map(|p| p.button);
                self.begin_grab(id, GrabKind::Move, button);
            }
            Request::Resize(edge) => {
                let button = self.press.map(|p| p.button);
                let edge = if edge == ResizeEdge::None {
                    grab::edge_for(self.windows[&id].rect, self.pointer.0, self.pointer.1)
                } else {
                    edge
                };
                self.begin_grab(id, GrabKind::Resize(edge), button);
            }
            Request::Activate => {
                if self.windows[&id].minimized {
                    self.set_minimized(id, false);
                }
                self.set_focus(Some(id));
            }
            Request::Close => self.out.push(Command::Close(id)),
            Request::ShowMenu { x, y } => {
                let rect = self.windows[&id].rect;
                self.open_menu("client-menu", (rect.x + x, rect.y + y), Some(id));
            }
            Request::Geometry(rect) => {
                let win = &self.windows[&id];
                if win.maximized || win.fullscreen {
                    return;
                }
                // A window that places itself gets exactly what it asks: a panel laying itself
                // along an edge or a desktop window growing over a new screen is never clamped
                // into the workarea its own strut shrank.
                let exact = places_itself(win.info.window_type);
                let extents = self.extents_of(id);
                let mut wanted = win.rect;
                if rect.w > 0 && rect.h > 0 {
                    wanted.w = rect.w;
                    wanted.h = rect.h;
                }
                if exact || (win.placed && (rect.x != win.rect.x || rect.y != win.rect.y)) {
                    wanted.x = rect.x;
                    wanted.y = rect.y;
                }
                let client = if exact {
                    wanted
                } else {
                    let area = self.workarea(win.output);
                    grab::client_of(place::clamp(wanted.grown(extents), area), extents)
                };
                if !win.placed {
                    if let Some(w) = self.windows.get_mut(&id) {
                        w.rect = client;
                        w.info.requested = Some(rect);
                        w.info.positioned = exact;
                    }
                    self.configure(id);
                } else {
                    self.set_rect(id, client);
                    self.render_frame(id);
                }
            }
            Request::Raise => self.raise(id),
            Request::Lower => self.lower(id),
            Request::Attention(on) => {
                if let Some(win) = self.windows.get_mut(&id) {
                    win.attention = on && self.focus != Some(id);
                }
            }
            Request::Above(on) => {
                if let Some(win) = self.windows.get_mut(&id) {
                    win.above = on;
                    if on {
                        win.below = false;
                    }
                }
                self.emit_stack();
            }
            Request::Below(on) => {
                if let Some(win) = self.windows.get_mut(&id) {
                    win.below = on;
                    if on {
                        win.above = false;
                    }
                }
                self.emit_stack();
            }
            Request::Shade(on) => self.set_shaded(id, on),
        }
    }

    // ---- grabs -------------------------------------------------------------------------------

    fn begin_grab(&mut self, id: WindowId, kind: GrabKind, button: Option<u32>) {
        let Some(win) = self.win(id) else {
            return;
        };
        if !win.shown() || win.fullscreen {
            return;
        }
        if win.maximized && matches!(kind, GrabKind::Resize(_)) {
            return;
        }
        if win.maximized {
            let pointer_x = self.pointer.0;
            let saved = win.saved.unwrap_or(win.rect);
            let rect = win.rect;
            let ratio = ((pointer_x - rect.x as f64) / rect.w.max(1) as f64).clamp(0.0, 1.0);
            self.set_maximized(id, false);
            let restored = self.windows[&id].rect;
            let x = (pointer_x - ratio * restored.w as f64).round() as i32;
            let _ = saved;
            self.set_rect(id, Rect::new(x, rect.y, restored.w, restored.h));
        }
        let rect = self.windows[&id].rect;
        self.grab = Some(Grab {
            window: id,
            kind,
            origin: self.press.map(|p| p.at).unwrap_or(self.pointer),
            start: rect,
            button,
        });
        self.set_cursor(match kind {
            GrabKind::Move => "grabbing",
            GrabKind::Resize(edge) => edge.cursor(),
        });
    }

    fn grab_motion(&mut self, x: f64, y: f64) {
        let Some(grab) = self.grab.clone() else {
            return;
        };
        let id = grab.window;
        let Some(win) = self.win(id) else {
            self.grab = None;
            return;
        };
        let extents = self.extents_of(id);
        let hints = win.info.hints;
        let output = win.output;
        let wanted = grab::dragged(&grab, x, y);
        let rect = match grab.kind {
            GrabKind::Move => {
                let area = self.workarea(output);
                let others: Vec<Rect> = self
                    .stack
                    .iter()
                    .filter(|w| **w != id)
                    .filter_map(|w| self.win(*w).filter(|c| c.shown() && c.listed()).map(|c| (*w, c)))
                    .map(|(w, _)| self.frame_rect(w))
                    .collect();
                let frame = grab::resist_move(
                    wanted.grown(extents),
                    area,
                    &others,
                    self.config.resistance,
                    self.config.screen_edge_resistance,
                );
                let frame = if frame.y < area.y {
                    frame.at(frame.x, area.y)
                } else {
                    frame
                };
                grab::client_of(frame, extents)
            }
            GrabKind::Resize(edge) => grab::constrain(wanted, &hints, edge),
        };
        if let Some(win) = self.windows.get_mut(&id) {
            win.rect = rect;
            win.resize_anchor = None;
            if let Some(o) = self
                .outputs
                .iter()
                .max_by_key(|o| o.rect.intersection(&rect).area())
                && o.rect.intersection(&rect).area() > 0
            {
                win.output = o.id;
            }
        }
        self.configure(id);
        self.render_frame(id);
    }

    fn end_grab(&mut self) {
        if let Some(grab) = self.grab.take() {
            let id = grab.window;
            if let GrabKind::Resize(edge) = grab.kind
                && let Some(win) = self.windows.get_mut(&id)
            {
                win.resize_anchor = Some(edge);
            }
            if let Some(rect) = self.win(id).map(|w| w.rect) {
                let output = self.output_of_rect(rect).map(|o| o.id);
                if let Some(output) = output
                    && let Some(win) = self.windows.get_mut(&id)
                {
                    win.output = output;
                }
            }
            self.render_frame(id);
        }
        self.update_hover();
    }

    // ---- pointer -----------------------------------------------------------------------------

    /// What lies under the pointer from the engine's side: a frame part of the topmost window
    /// whose frame holds the point, else the client the compositor named.
    fn hit(&self, x: f64, y: f64, under: &Under) -> (Context, Option<WindowId>) {
        for id in self.ordered_stack().into_iter().rev() {
            let Some(win) = self.win(id) else {
                continue;
            };
            if !win.decorated || win.fullscreen {
                continue;
            }
            let frame = self.frame_rect(id);
            if !frame.contains_f64(x, y) {
                continue;
            }
            let layout = frame::layout(
                &self.theme.look.style,
                Size {
                    w: win.rect.w,
                    h: win.rect.h,
                },
                win.maximized,
                win.shaded,
            );
            let lx = x as i32 - frame.x;
            let ly = y as i32 - frame.y;
            if let Some(context) = frame::hit(&layout, lx, ly, win.shaded) {
                return (context, Some(id));
            }
            if win.rect.contains_f64(x, y) {
                return (Context::Client, Some(id));
            }
        }
        match under
            .window
            .filter(|w| self.win(*w).is_some_and(|c| c.shown() && !c.shaded))
        {
            Some(id) => {
                if self.windows[&id].info.window_type == WindowType::Desktop {
                    (Context::Desktop, Some(id))
                } else {
                    (Context::Client, Some(id))
                }
            }
            None if under.desktop => (Context::Desktop, None),
            None => (Context::Root, None),
        }
    }

    fn pointer(&mut self, event: PointerEvent, under: Under) -> Verdict {
        self.pointer = (event.x, event.y);
        let button_number = |button: u32| match button {
            BTN_LEFT => 1,
            BTN_MIDDLE => 2,
            BTN_RIGHT => 3,
            other => other.saturating_sub(0x110) + 1,
        };
        if self.menu.is_some() {
            return self.menu_pointer(event, button_number);
        }
        if let Some(grab) = self.grab.clone() {
            match event.kind {
                PointerKind::Motion => self.grab_motion(event.x, event.y),
                PointerKind::Button {
                    button,
                    pressed: false,
                } if grab.button.is_none_or(|b| b == button_number(button)) => {
                    self.end_grab();
                    self.press = None;
                }
                PointerKind::Button { pressed: true, .. } if grab.button.is_none() => {
                    self.end_grab();
                }
                _ => {}
            }
            return Verdict::Consumed;
        }
        match event.kind {
            PointerKind::Motion => {
                if let Some(press) = self.press {
                    let dx = event.x - press.at.0;
                    let dy = event.y - press.at.1;
                    let threshold = self.config.drag_threshold.max(1) as f64;
                    if !press.dragged && (dx.abs() >= threshold || dy.abs() >= threshold) {
                        if let Some(p) = self.press.as_mut() {
                            p.dragged = true;
                        }
                        let ran = self.run_mousebinds(
                            press.context,
                            press.button,
                            event.modifiers,
                            MouseEvent::Drag,
                            press.window,
                        );
                        if ran && self.grab.is_some() {
                            self.grab_motion(event.x, event.y);
                        }
                        return if press.consumed || ran {
                            Verdict::Consumed
                        } else {
                            Verdict::Pass
                        };
                    }
                    return if press.consumed {
                        Verdict::Consumed
                    } else {
                        Verdict::Pass
                    };
                }
                if under.top_layer {
                    self.clear_hover();
                    self.set_cursor("default");
                    return Verdict::Pass;
                }
                let (context, window) = self.hit(event.x, event.y, &under);
                self.set_hover(context, window);
                if matches!(context, Context::Client | Context::Root | Context::Desktop) {
                    Verdict::Pass
                } else {
                    Verdict::Consumed
                }
            }
            PointerKind::Button {
                button,
                pressed: true,
            } => {
                if under.top_layer && self.hit(event.x, event.y, &under).0 == Context::Root {
                    return Verdict::Pass;
                }
                let (context, window) = self.hit(event.x, event.y, &under);
                let number = button_number(button);
                if let Some(id) = window
                    && !matches!(context, Context::Client | Context::Desktop)
                    && let Some(win) = self.windows.get_mut(&id)
                {
                    win.pressed = Some(context);
                }
                let ran = self.run_mousebinds(context, number, event.modifiers, MouseEvent::Press, window);
                let consumed = match context {
                    Context::Client | Context::Desktop => {
                        bind_modifiers(event.modifiers) != Modifiers::default() && ran
                    }
                    Context::Root => ran,
                    _ => true,
                };
                self.press = Some(Press {
                    button: number,
                    context,
                    window,
                    at: (event.x, event.y),
                    dragged: false,
                    consumed,
                });
                if let Some(id) = window {
                    self.render_frame(id);
                }
                if consumed {
                    Verdict::Consumed
                } else {
                    Verdict::Pass
                }
            }
            PointerKind::Button {
                button,
                pressed: false,
            } => {
                let number = button_number(button);
                let Some(press) = self.press.take().filter(|p| p.button == number) else {
                    return Verdict::Pass;
                };
                if let Some(id) = press.window
                    && let Some(win) = self.windows.get_mut(&id)
                {
                    win.pressed = None;
                }
                let (context, window) = self.hit(event.x, event.y, &under);
                let same = context == press.context && window == press.window;
                self.run_mousebinds(
                    press.context,
                    number,
                    event.modifiers,
                    MouseEvent::Release,
                    press.window,
                );
                if !press.dragged && same {
                    self.run_mousebinds(
                        press.context,
                        number,
                        event.modifiers,
                        MouseEvent::Click,
                        press.window,
                    );
                    let double = self.last_click.is_some_and(|(b, c, w, t)| {
                        b == number
                            && c == press.context
                            && w == press.window
                            && event.time.wrapping_sub(t) <= self.config.double_click_ms
                    });
                    if double {
                        self.run_mousebinds(
                            press.context,
                            number,
                            event.modifiers,
                            MouseEvent::DoubleClick,
                            press.window,
                        );
                        self.last_click = None;
                    } else {
                        self.last_click = Some((number, press.context, press.window, event.time));
                    }
                }
                if let Some(id) = press.window {
                    self.render_frame(id);
                }
                self.update_hover();
                if press.consumed {
                    Verdict::Consumed
                } else {
                    Verdict::Pass
                }
            }
            PointerKind::Axis { .. } => {
                let (context, _) = self.hit(event.x, event.y, &under);
                if matches!(context, Context::Client | Context::Root | Context::Desktop) || under.top_layer {
                    Verdict::Pass
                } else {
                    Verdict::Consumed
                }
            }
        }
    }

    fn set_hover(&mut self, context: Context, window: Option<WindowId>) {
        let button = matches!(
            context,
            Context::Iconify | Context::Maximize | Context::Close | Context::Shade | Context::Icon
        );
        let hovered = if button { window } else { None };
        let ids: Vec<WindowId> = self.windows.keys().copied().collect();
        for id in ids {
            let want = if hovered == Some(id) { Some(context) } else { None };
            if self.windows[&id].hover != want {
                if let Some(win) = self.windows.get_mut(&id) {
                    win.hover = want;
                }
                self.render_frame(id);
            }
        }
        let cursor = match context {
            Context::Top => ResizeEdge::Top.cursor(),
            Context::Bottom => ResizeEdge::Bottom.cursor(),
            Context::Left => ResizeEdge::Left.cursor(),
            Context::Right => ResizeEdge::Right.cursor(),
            Context::TLCorner => ResizeEdge::TopLeft.cursor(),
            Context::TRCorner => ResizeEdge::TopRight.cursor(),
            Context::BLCorner => ResizeEdge::BottomLeft.cursor(),
            Context::BRCorner => ResizeEdge::BottomRight.cursor(),
            _ => "default",
        };
        self.set_cursor(cursor);
    }

    fn clear_hover(&mut self) {
        let ids: Vec<WindowId> = self
            .windows
            .iter()
            .filter(|(_, w)| w.hover.is_some())
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            if let Some(win) = self.windows.get_mut(&id) {
                win.hover = None;
            }
            self.render_frame(id);
        }
    }

    fn update_hover(&mut self) {
        let (x, y) = self.pointer;
        let (context, window) = self.hit(x, y, &Under::default());
        self.set_hover(context, window);
    }

    /// Run the bindings of a context for a button event; whether any matched.
    fn run_mousebinds(
        &mut self,
        context: Context,
        button: u32,
        modifiers: Modifiers,
        event: MouseEvent,
        window: Option<WindowId>,
    ) -> bool {
        let modifiers = bind_modifiers(modifiers);
        let contexts: Vec<Context> = match context {
            Context::Client
            | Context::Titlebar
            | Context::Top
            | Context::Bottom
            | Context::Left
            | Context::Right
            | Context::TLCorner
            | Context::TRCorner
            | Context::BLCorner
            | Context::BRCorner
            | Context::Icon
            | Context::Iconify
            | Context::Maximize
            | Context::Close
            | Context::Shade
            | Context::AllDesktops => {
                vec![context, Context::Frame]
            }
            other => vec![other],
        };
        let mut matched = Vec::new();
        for ctx in contexts {
            for bind in &self.config.mousebinds {
                if bind.context == ctx
                    && bind.button == button
                    && bind.modifiers == modifiers
                    && bind.event == event
                {
                    matched.push(bind.actions.clone());
                }
            }
            if !matched.is_empty() && ctx != Context::Frame {
                break;
            }
        }
        if matched.is_empty() {
            return false;
        }
        let at = self.pointer;
        for actions in matched {
            self.run_actions(&actions, window, at);
        }
        true
    }

    // ---- keyboard -----------------------------------------------------------------------------

    fn key(&mut self, event: KeyEvent) -> Verdict {
        if !event.pressed {
            if self.consumed_keys.remove(&event.keycode) {
                self.after_modifier_release(&event);
                return Verdict::Consumed;
            }
            if let Some(bind) = self.release_bind.take()
                && lower_keysym(event.base_keysym) == bind.keysym
            {
                let focus = self.focus;
                self.run_actions(&bind.actions, focus, self.pointer);
                return Verdict::Pass;
            }
            self.after_modifier_release(&event);
            return Verdict::Pass;
        }
        if self.release_bind.is_some() && !is_modifier(event.keysym) {
            self.release_bind = None;
        }
        if self.menu.is_some() {
            self.consumed_keys.insert(event.keycode);
            self.menu_key(&event);
            return Verdict::Consumed;
        }
        if self.cycle.is_some() {
            self.consumed_keys.insert(event.keycode);
            self.cycle_key(&event);
            return Verdict::Consumed;
        }
        if self.grab.is_some() && event.keysym == keys::ESCAPE {
            let grab = self.grab.take();
            if let Some(grab) = grab {
                self.set_rect(grab.window, grab.start);
                self.render_frame(grab.window);
            }
            self.set_cursor("default");
            self.consumed_keys.insert(event.keycode);
            return Verdict::Consumed;
        }
        let modifiers = bind_modifiers(event.modifiers);
        let keysym = lower_keysym(event.base_keysym);
        let bind = self
            .config
            .keybinds
            .iter()
            .find(|b| b.keysym == keysym && b.modifiers == modifiers)
            .cloned();
        let Some(bind) = bind else {
            return Verdict::Pass;
        };
        if bind.on_release {
            self.release_bind = Some(bind);
            return Verdict::Pass;
        }
        self.consumed_keys.insert(event.keycode);
        let focus = self.focus;
        self.run_actions(&bind.actions, focus, self.pointer);
        Verdict::Consumed
    }

    /// A modifier's release ends a cycle whose modifiers are no longer held.
    fn after_modifier_release(&mut self, event: &KeyEvent) {
        if let Some(cycle) = &self.cycle {
            let held = bind_modifiers(event.modifiers);
            let needed = cycle.modifiers;
            let still = (!needed.alt || held.alt)
                && (!needed.logo || held.logo)
                && (!needed.ctrl || held.ctrl)
                && (!needed.shift
                    || held.shift
                    || is_modifier(event.keysym)
                        && event.keysym != keys::SHIFT_L
                        && event.keysym != keys::SHIFT_R);
            if !still || needed == Modifiers::default() {
                self.finish_cycle(true);
            }
        }
    }

    // ---- actions --------------------------------------------------------------------------------

    /// Run actions on a window (the focused one for a key bind, the one under the pointer for a
    /// mouse bind), the pointer at `at`.
    fn run_actions(&mut self, actions: &[Action], window: Option<WindowId>, at: (f64, f64)) {
        for action in actions {
            self.run_action(action, window, at);
        }
    }

    fn run_action(&mut self, action: &Action, window: Option<WindowId>, at: (f64, f64)) {
        let target = window.filter(|w| self.win(*w).is_some());
        match action {
            Action::Execute(command) => self.launcher.spawn(command),
            Action::ShowMenu(id) => {
                let id = id.clone();
                self.open_menu(&id, (at.0 as i32, at.1 as i32), target);
            }
            Action::Close | Action::Kill => {
                if let Some(id) = target {
                    self.out.push(Command::Close(id));
                }
            }
            Action::Iconify => {
                if let Some(id) = target {
                    self.set_minimized(id, true);
                }
            }
            Action::Focus => {
                if let Some(id) = target
                    && self.windows[&id].focusable
                {
                    self.set_focus(Some(id));
                }
            }
            Action::Unfocus => {
                if self.focus == target && target.is_some() {
                    self.focus = None;
                    self.out.push(Command::Focus(None));
                    if let Some(id) = target {
                        self.configure(id);
                        self.render_frame(id);
                    }
                    self.emit_stack();
                }
            }
            Action::Raise => {
                if let Some(id) = target {
                    self.raise(id);
                }
            }
            Action::Lower => {
                if let Some(id) = target {
                    self.lower(id);
                }
            }
            Action::RaiseLower => {
                if let Some(id) = target {
                    if self.ordered_stack().last() == Some(&id) {
                        self.lower(id);
                    } else {
                        self.raise(id);
                    }
                }
            }
            Action::FocusToBottom => {
                if let Some(id) = target {
                    self.mru.retain(|w| *w != id);
                    self.mru.push(id);
                }
            }
            Action::Maximize(_) => {
                if let Some(id) = target {
                    self.set_maximized(id, true);
                }
            }
            Action::Unmaximize(_) => {
                if let Some(id) = target {
                    self.set_maximized(id, false);
                }
            }
            Action::ToggleMaximize(_) => {
                if let Some(id) = target {
                    let on = !self.windows[&id].maximized;
                    self.set_maximized(id, on);
                }
            }
            Action::Fullscreen => {
                if let Some(id) = target {
                    self.set_fullscreen(id, true, None);
                }
            }
            Action::Unfullscreen => {
                if let Some(id) = target {
                    self.set_fullscreen(id, false, None);
                }
            }
            Action::ToggleFullscreen => {
                if let Some(id) = target {
                    let on = !self.windows[&id].fullscreen;
                    self.set_fullscreen(id, on, None);
                }
            }
            Action::Shade => {
                if let Some(id) = target {
                    self.set_shaded(id, true);
                }
            }
            Action::Unshade => {
                if let Some(id) = target {
                    self.set_shaded(id, false);
                }
            }
            Action::ToggleShade => {
                if let Some(id) = target {
                    let on = !self.windows[&id].shaded;
                    self.set_shaded(id, on);
                }
            }
            Action::Decorate => {
                if let Some(id) = target {
                    self.set_decorated(id, true);
                }
            }
            Action::Undecorate => {
                if let Some(id) = target {
                    self.set_decorated(id, false);
                }
            }
            Action::ToggleDecorations => {
                if let Some(id) = target {
                    let on = !self.windows[&id].decorated;
                    self.set_decorated(id, on);
                }
            }
            Action::ToggleAlwaysOnTop => {
                if let Some(id) = target {
                    let on = !self.windows[&id].above;
                    self.request(id, Request::Above(on));
                }
            }
            Action::ToggleAlwaysOnBottom => {
                if let Some(id) = target {
                    let on = !self.windows[&id].below;
                    self.request(id, Request::Below(on));
                }
            }
            Action::Move => {
                if let Some(id) = target {
                    let button = self.press.map(|p| p.button);
                    self.begin_grab(id, GrabKind::Move, button);
                }
            }
            Action::Resize(edge) => {
                if let Some(id) = target {
                    let button = self.press.map(|p| p.button);
                    let edge = edge.unwrap_or_else(|| grab::edge_for(self.windows[&id].rect, at.0, at.1));
                    self.begin_grab(id, GrabKind::Resize(edge), button);
                }
            }
            Action::MoveResizeTo { x, y, width, height } => {
                if let Some(id) = target {
                    let win = &self.windows[&id];
                    let extents = self.extents_of(id);
                    let area = self.workarea(win.output);
                    let mut frame = win.rect.grown(extents);
                    if let Some(w) = width {
                        frame.w = *w + extents.left + extents.right;
                    }
                    if let Some(h) = height {
                        frame.h = *h + extents.top + extents.bottom;
                    }
                    if let Some(x) = x {
                        frame.x = x.resolve(area.x, area.w, frame.w);
                    }
                    if let Some(y) = y {
                        frame.y = y.resolve(area.y, area.h, frame.h);
                    }
                    self.set_rect(id, grab::client_of(frame, extents));
                    self.render_frame(id);
                }
            }
            Action::MoveToEdge(edge) => {
                if let Some(id) = target {
                    let extents = self.extents_of(id);
                    let area = self.workarea(self.windows[&id].output);
                    let mut frame = self.windows[&id].rect.grown(extents);
                    if edge.left() {
                        frame.x = area.x;
                    }
                    if edge.right() {
                        frame.x = area.right() - frame.w;
                    }
                    if edge.top() {
                        frame.y = area.y;
                    }
                    if edge.bottom() {
                        frame.y = area.bottom() - frame.h;
                    }
                    self.set_rect(id, grab::client_of(frame, extents));
                    self.render_frame(id);
                }
            }
            Action::NextWindow => self.cycle_step(1),
            Action::PreviousWindow => self.cycle_step(-1),
            Action::ShowDesktop => self.show_desktop(true),
            Action::ToggleShowDesktop => {
                let on = !self.showing_desktop;
                self.show_desktop(on);
            }
            Action::Reconfigure => self.reconfigure(),
            Action::Unknown(name) => {
                if self.unknown_logged.insert(name.clone()) {
                    log::warn!(target: "wm", "action {name:?} is not implemented");
                }
            }
        }
    }

    fn show_desktop(&mut self, on: bool) {
        if self.showing_desktop == on {
            return;
        }
        self.showing_desktop = on;
        let ids: Vec<WindowId> = self.stack.clone();
        if on {
            for id in ids {
                if self.win(id).is_some_and(|w| w.shown() && w.listed()) {
                    if let Some(win) = self.windows.get_mut(&id) {
                        win.by_show_desktop = true;
                    }
                    self.set_minimized(id, true);
                }
            }
        } else {
            let restore: Vec<WindowId> = self
                .windows
                .iter()
                .filter(|(_, w)| w.minimized && w.by_show_desktop)
                .map(|(id, _)| *id)
                .collect();
            for id in restore {
                self.set_minimized(id, false);
            }
        }
    }

    // ---- Alt-Tab -------------------------------------------------------------------------------

    fn cycle_step(&mut self, step: i32) {
        if self.cycle.is_none() {
            let mut windows: Vec<WindowId> = self
                .mru
                .iter()
                .copied()
                .filter(|w| {
                    self.win(*w)
                        .is_some_and(|c| c.mapped && c.listed() && c.focusable)
                })
                .collect();
            for id in self.stack.clone() {
                if !windows.contains(&id)
                    && self
                        .win(id)
                        .is_some_and(|c| c.mapped && c.listed() && c.focusable)
                {
                    windows.push(id);
                }
            }
            for (id, win) in &self.windows {
                if win.minimized && win.mapped && win.listed() && win.focusable && !windows.contains(id) {
                    windows.push(*id);
                }
            }
            if windows.is_empty() {
                return;
            }
            let modifiers = self
                .config
                .keybinds
                .iter()
                .find(|b| {
                    matches!(
                        b.actions.first(),
                        Some(Action::NextWindow | Action::PreviousWindow)
                    )
                })
                .map(|b| Modifiers {
                    shift: false,
                    ..b.modifiers
                })
                .unwrap_or(Modifiers {
                    alt: true,
                    ..Modifiers::default()
                });
            self.cycle = Some(Cycle {
                windows,
                index: 0,
                modifiers,
            });
        }
        if let Some(cycle) = self.cycle.as_mut() {
            let n = cycle.windows.len() as i32;
            cycle.index = ((cycle.index as i32 + step).rem_euclid(n)) as usize;
        }
        self.render_cycle();
    }

    fn cycle_key(&mut self, event: &KeyEvent) {
        match event.keysym {
            keys::ESCAPE => self.finish_cycle(false),
            keys::RETURN | keys::KP_ENTER | keys::SPACE => self.finish_cycle(true),
            _ => {
                let keysym = lower_keysym(event.base_keysym);
                let modifiers = bind_modifiers(event.modifiers);
                let bind = self
                    .config
                    .keybinds
                    .iter()
                    .find(|b| b.keysym == keysym && b.modifiers == modifiers)
                    .cloned();
                match bind.as_ref().and_then(|b| b.actions.first()) {
                    Some(Action::NextWindow) => self.cycle_step(1),
                    Some(Action::PreviousWindow) => self.cycle_step(-1),
                    _ => {
                        if event.keysym == keys::DOWN || event.keysym == keys::RIGHT {
                            self.cycle_step(1);
                        } else if event.keysym == keys::UP || event.keysym == keys::LEFT {
                            self.cycle_step(-1);
                        }
                    }
                }
            }
        }
    }

    fn finish_cycle(&mut self, apply: bool) {
        let Some(cycle) = self.cycle.take() else {
            return;
        };
        self.out.push(Command::ClearOverlay(CYCLE_KEY));
        if apply && let Some(id) = cycle.windows.get(cycle.index).copied() {
            if self.win(id).is_some_and(|w| w.minimized) {
                self.set_minimized(id, false);
            }
            self.set_focus(Some(id));
        }
    }

    fn render_cycle(&mut self) {
        let Some(cycle) = &self.cycle else {
            return;
        };
        let output = self
            .focus
            .and_then(|f| self.win(f))
            .map(|w| w.output)
            .unwrap_or_else(|| {
                self.output_at(self.pointer.0, self.pointer.1)
                    .map(|o| o.id)
                    .unwrap_or(0)
            });
        let area = self.workarea(output);
        let scale = self.scale_of(output);
        let entries: Vec<Entry> = cycle
            .windows
            .iter()
            .map(|id| {
                let win = &self.windows[id];
                Entry::Item {
                    label: if win.info.title.is_empty() {
                        win.info.app_id.clone()
                    } else {
                        win.info.title.clone()
                    },
                    icon: Some(icon_name(&win.info.app_id)),
                    actions: Vec::new(),
                    window: Some(*id),
                    enabled: true,
                    checked: false,
                }
            })
            .collect();
        let index = cycle.index;
        let hint = menu::size_hint(&entries);
        let mut level = Level::new(
            &mut self.renderer,
            "cycle".to_string(),
            entries,
            (area.x + (area.w - hint.w) / 2, area.y + (area.h - hint.h) / 2),
            area,
            scale,
            CYCLE_KEY,
        );
        level.rect.x = area.x + (area.w - level.rect.w) / 2;
        level.rect.y = area.y + (area.h - level.rect.h) / 2;
        level.selected = Some(index);
        let look = self.theme.look;
        if let Some(image) = level.render(&mut self.renderer, &look, scale) {
            self.out.push(Command::Overlay {
                key: CYCLE_KEY,
                rect: level.rect,
                image,
            });
        }
    }

    // ---- menus ----------------------------------------------------------------------------------

    /// The entries of a menu by id: the window menu and the window list built here, the others
    /// from the files, a pipe menu's from its command once it answered.
    fn entries_for(&mut self, id: &str, window: Option<WindowId>) -> (Vec<Entry>, bool) {
        match id {
            "client-menu" => (self.client_menu(window), false),
            "client-list-combined-menu" | "client-list-menu" => (self.client_list(), false),
            APPLICATIONS_MENU => (self.applications_categories(), false),
            _ if id.starts_with(APPLICATIONS_CATEGORY_PREFIX) => {
                let category = id[APPLICATIONS_CATEGORY_PREFIX.len()..].to_string();
                (self.applications_in(&category), false)
            }
            "root-menu" if !self.menus.menus.contains_key(id) => (self.default_root_menu(), false),
            _ => {
                let Some(menu) = self.menus.menus.get(id).cloned() else {
                    return (
                        vec![Entry::Item {
                            label: id.to_string(),
                            icon: None,
                            actions: Vec::new(),
                            window: None,
                            enabled: false,
                            checked: false,
                        }],
                        false,
                    );
                };
                if let Some(command) = &menu.execute {
                    if !self.pipes.iter().any(|p| p.menu_id == id) {
                        self.pipes.push(PipeJob::spawn(
                            id.to_string(),
                            command.clone(),
                            self.launcher.env(),
                        ));
                    }
                    if menu.items.is_empty() {
                        return (
                            vec![Entry::Item {
                                label: tr("Loading…").to_string(),
                                icon: None,
                                actions: Vec::new(),
                                window: None,
                                enabled: false,
                                checked: false,
                            }],
                            true,
                        );
                    }
                }
                let show_icons = self.config.show_menu_icons;
                let wants_icon = show_icons
                    && menu.items.iter().any(|item| {
                        matches!(item, MenuItem::Item { icon: None, actions, .. }
                            if actions.iter().any(|a| matches!(a, Action::Execute(_))))
                    });
                if wants_icon {
                    self.apps();
                }
                let entries = menu
                    .items
                    .iter()
                    .map(|item| match item {
                        MenuItem::Item { label, icon, actions } => Entry::Item {
                            label: label.clone(),
                            icon: icon
                                .clone()
                                .or_else(|| {
                                    let command = actions.iter().find_map(|a| match a {
                                        Action::Execute(command) => Some(command.as_str()),
                                        _ => None,
                                    })?;
                                    let apps = self.apps.as_ref()?;
                                    apps::icon_for_command(&apps.entries, command)
                                })
                                .filter(|_| show_icons),
                            actions: actions.clone(),
                            window,
                            enabled: true,
                            checked: false,
                        },
                        MenuItem::Separator { label } => Entry::Separator { label: label.clone() },
                        MenuItem::Submenu { id, label, icon } => Entry::Submenu {
                            label: label
                                .clone()
                                .or_else(|| {
                                    self.menus
                                        .menus
                                        .get(id)
                                        .map(|m| m.label.clone())
                                        .filter(|l| !l.is_empty())
                                })
                                .unwrap_or_else(|| match id.as_str() {
                                    "client-list-combined-menu" | "client-list-menu" => {
                                        tr("Windows").to_string()
                                    }
                                    APPLICATIONS_MENU => tr("Applications").to_string(),
                                    other => other.to_string(),
                                }),
                            icon: icon
                                .clone()
                                .or_else(|| self.menus.menus.get(id).and_then(|m| m.icon.clone()))
                                .filter(|_| self.config.show_menu_icons),
                            target: Target::Menu(id.clone()),
                        },
                    })
                    .collect();
                (entries, menu.execute.is_some() && menu.items.is_empty())
            }
        }
    }

    /// The desktop entries, read on first use and again when an applications directory changed
    /// since, so an application installed in the session is listed the next time the menu opens.
    fn apps(&mut self) -> &Apps {
        if self.apps.as_ref().is_none_or(Apps::stale) {
            self.apps = Some(Apps::load(apps::find_terminal(None).is_some()));
        }
        self.apps.as_ref().expect("the desktop entries were just read")
    }

    /// The applications menu's top level: one submenu per category, the miscellaneous one last.
    fn applications_categories(&mut self) -> Vec<Entry> {
        let show_icons = self.config.show_menu_icons;
        let entries: Vec<Entry> = self
            .apps()
            .categories
            .iter()
            .map(|category| Entry::Submenu {
                label: tr(category_key(&category.name)).to_string(),
                icon: show_icons.then(|| apps::category_icon(&category.name).to_string()),
                target: Target::Menu(format!("{APPLICATIONS_CATEGORY_PREFIX}{}", category.name)),
            })
            .collect();
        if entries.is_empty() {
            return vec![Entry::Item {
                label: tr("Applications").to_string(),
                icon: None,
                actions: Vec::new(),
                window: None,
                enabled: false,
                checked: false,
            }];
        }
        entries
    }

    /// The applications of one category, each started through its Exec line with the field
    /// codes expanded, in the session's terminal where it asks for one, in its working directory
    /// where it names one.
    fn applications_in(&mut self, category: &str) -> Vec<Entry> {
        let show_icons = self.config.show_menu_icons;
        let terminal = apps::find_terminal(None);
        let apps = self.apps();
        let Some(category) = apps.categories.iter().find(|c| c.name == category) else {
            return Vec::new();
        };
        category
            .entries
            .iter()
            .filter_map(|index| apps.entries.get(*index))
            .map(|entry| {
                let mut argv = apps::exec_argv(&entry.exec, entry.icon.as_deref(), &entry.name, &entry.path);
                if entry.terminal
                    && let Some(terminal) = &terminal
                {
                    let mut wrapped = terminal.clone();
                    wrapped.append(&mut argv);
                    argv = wrapped;
                }
                let mut command = apps::shell_command(&argv);
                if let Some(dir) = &entry.working_dir {
                    command = format!(
                        "cd {} && {command}",
                        apps::shell_command(std::slice::from_ref(dir))
                    );
                }
                Entry::Item {
                    label: entry.name.clone(),
                    icon: entry.icon.clone().filter(|_| show_icons),
                    actions: vec![Action::Execute(command)],
                    window: None,
                    enabled: !argv.is_empty(),
                    checked: false,
                }
            })
            .collect()
    }

    /// The desktop's menu where no menu file defines one: the applications and the windows.
    fn default_root_menu(&self) -> Vec<Entry> {
        let show_icons = self.config.show_menu_icons;
        vec![
            Entry::Submenu {
                label: tr("Applications").to_string(),
                icon: show_icons.then(|| "applications-all".to_string()),
                target: Target::Menu(APPLICATIONS_MENU.to_string()),
            },
            Entry::Submenu {
                label: tr("Windows").to_string(),
                icon: None,
                target: Target::Menu("client-list-combined-menu".to_string()),
            },
        ]
    }

    fn client_menu(&self, window: Option<WindowId>) -> Vec<Entry> {
        let Some(id) = window else {
            return Vec::new();
        };
        let Some(win) = self.win(id) else {
            return Vec::new();
        };
        let item = |label: &'static str, actions: Vec<Action>, enabled: bool, checked: bool| Entry::Item {
            label: tr(label).to_string(),
            icon: None,
            actions,
            window: Some(id),
            enabled,
            checked,
        };
        let mut entries = Vec::new();
        if win.maximized {
            entries.push(item(
                "Restore",
                vec![Action::Unmaximize(Direction::Both)],
                true,
                false,
            ));
        } else {
            entries.push(item(
                "Maximize",
                vec![Action::Maximize(Direction::Both)],
                !win.fullscreen,
                false,
            ));
        }
        entries.push(item("Minimize", vec![Action::Iconify], true, false));
        entries.push(item("Move", vec![Action::Move], !win.fullscreen, false));
        entries.push(item(
            "Resize",
            vec![Action::Resize(None)],
            !win.fullscreen && !win.maximized,
            false,
        ));
        entries.push(Entry::Separator { label: None });
        entries.push(item(
            "Always on top",
            vec![Action::ToggleAlwaysOnTop],
            true,
            win.above,
        ));
        if win.decorated {
            entries.push(item(
                if win.shaded { "Unshade" } else { "Shade" },
                vec![Action::ToggleShade],
                !win.fullscreen,
                false,
            ));
        }
        entries.push(item(
            "Decorations",
            vec![Action::ToggleDecorations],
            !win.fullscreen,
            win.decorated,
        ));
        entries.push(Entry::Separator { label: None });
        entries.push(item("Close", vec![Action::Close], true, false));
        entries
    }

    fn client_list(&self) -> Vec<Entry> {
        let mut entries = vec![Entry::Separator {
            label: Some(tr("Windows").to_string()),
        }];
        let mut listed: Vec<&WindowId> = self.stack.iter().rev().collect();
        for (id, win) in &self.windows {
            if win.minimized && win.mapped && !listed.contains(&id) {
                listed.push(id);
            }
        }
        let mut any = false;
        for id in listed {
            let win = &self.windows[id];
            if !win.listed() || !win.mapped {
                continue;
            }
            any = true;
            let label = if win.info.title.is_empty() {
                win.info.app_id.clone()
            } else {
                win.info.title.clone()
            };
            entries.push(Entry::Item {
                label: if win.minimized {
                    format!("({label})")
                } else {
                    label
                },
                icon: self.config.show_menu_icons.then(|| icon_name(&win.info.app_id)),
                actions: vec![Action::Focus],
                window: Some(*id),
                enabled: true,
                checked: self.focus == Some(*id),
            });
        }
        if !any {
            entries.pop();
        }
        entries
    }

    fn open_menu(&mut self, id: &str, at: (i32, i32), window: Option<WindowId>) {
        self.close_menu();
        let output = self
            .output_at(at.0 as f64, at.1 as f64)
            .map(|o| o.id)
            .unwrap_or(0);
        let area = self.workarea(output);
        let scale = self.scale_of(output);
        let (entries, loading) = self.entries_for(id, window);
        if entries.is_empty() {
            return;
        }
        let mut level = Level::new(
            &mut self.renderer,
            id.to_string(),
            entries,
            at,
            area,
            scale,
            MENU_KEY,
        );
        level.loading = loading;
        let look = self.theme.look;
        if let Some(image) = level.render(&mut self.renderer, &look, scale) {
            self.out.push(Command::Overlay {
                key: level.key,
                rect: level.rect,
                image,
            });
        }
        self.menu = Some(MenuState {
            levels: vec![level],
            window,
            output,
            opening_button: self.press.map(|p| p.button),
        });
        if loading {
            self.out
                .push(Command::WakeAt(self.now + Duration::from_millis(50)));
        }
    }

    fn close_menu(&mut self) {
        if let Some(menu) = self.menu.take() {
            for level in &menu.levels {
                self.out.push(Command::ClearOverlay(level.key));
            }
        }
        self.pipes.clear();
    }

    fn redraw_level(&mut self, depth: usize) {
        let Some(menu) = &self.menu else {
            return;
        };
        let Some(level) = menu.levels.get(depth) else {
            return;
        };
        let scale = self.scale_of(menu.output);
        let look = self.theme.look;
        let level = level.clone();
        if let Some(image) = level.render(&mut self.renderer, &look, scale) {
            self.out.push(Command::Overlay {
                key: level.key,
                rect: level.rect,
                image,
            });
        }
    }

    /// Close the levels deeper than `depth`.
    fn truncate_menu(&mut self, depth: usize) {
        let Some(menu) = self.menu.as_mut() else {
            return;
        };
        let removed: Vec<u32> = menu.levels.drain(depth + 1..).map(|l| l.key).collect();
        if let Some(level) = menu.levels.get_mut(depth) {
            level.open = None;
        }
        for key in removed {
            self.out.push(Command::ClearOverlay(key));
        }
    }

    fn open_submenu(&mut self, depth: usize, index: usize) {
        let Some(menu) = &self.menu else {
            return;
        };
        if menu.levels.get(depth).and_then(|l| l.open) == Some(index) {
            return;
        }
        self.truncate_menu(depth);
        let menu = self.menu.as_ref().unwrap();
        let level = &menu.levels[depth];
        let Entry::Submenu {
            target: Target::Menu(id),
            ..
        } = &level.entries[index]
        else {
            return;
        };
        let id = id.clone();
        let window = menu.window;
        let output = menu.output;
        let area = self.workarea(output);
        let scale = self.scale_of(output);
        let (entries, loading) = self.entries_for(&id, window);
        if entries.is_empty() {
            return;
        }
        let menu = self.menu.as_ref().unwrap();
        let parent = &menu.levels[depth];
        let hint = menu::size_hint(&entries);
        let at = menu::submenu_position(parent, index, hint.w, area);
        let key = MENU_KEY + depth as u32 + 1;
        let mut level = Level::new(&mut self.renderer, id, entries, at, area, scale, key);
        level.loading = loading;
        let look = self.theme.look;
        if let Some(image) = level.render(&mut self.renderer, &look, scale) {
            self.out.push(Command::Overlay {
                key,
                rect: level.rect,
                image,
            });
        }
        if let Some(menu) = self.menu.as_mut() {
            if let Some(parent) = menu.levels.get_mut(depth) {
                parent.open = Some(index);
                parent.selected = Some(index);
            }
            menu.levels.push(level);
        }
        self.redraw_level(depth);
        if loading {
            self.out
                .push(Command::WakeAt(self.now + Duration::from_millis(50)));
        }
    }

    fn activate_entry(&mut self, depth: usize, index: usize) {
        let Some(menu) = &self.menu else {
            return;
        };
        let Some(level) = menu.levels.get(depth) else {
            return;
        };
        match &level.entries[index] {
            Entry::Item {
                actions,
                window,
                enabled,
                ..
            } if *enabled => {
                let actions = actions.clone();
                let window = window.or(menu.window);
                let at = self.pointer;
                self.close_menu();
                self.run_actions(&actions, window, at);
            }
            Entry::Submenu { .. } => self.open_submenu(depth, index),
            _ => {}
        }
    }

    fn menu_pointer(&mut self, event: PointerEvent, button_number: impl Fn(u32) -> u32) -> Verdict {
        let Some(menu) = &self.menu else {
            return Verdict::Pass;
        };
        let hit = menu.levels.iter().enumerate().rev().find_map(|(depth, level)| {
            level
                .entry_at(event.x, event.y)
                .map(|i| (depth, Some(i)))
                .or_else(|| level.rect.contains_f64(event.x, event.y).then_some((depth, None)))
        });
        match event.kind {
            PointerKind::Motion => {
                if let Some((depth, Some(index))) = hit {
                    let selectable = menu.levels[depth].entries[index].selectable();
                    let current = menu.levels[depth].selected;
                    if current != Some(index) {
                        if let Some(menu) = self.menu.as_mut() {
                            menu.levels[depth].selected = selectable.then_some(index);
                        }
                        if matches!(menu_entry(self, depth, index), Some(Entry::Submenu { .. })) {
                            self.open_submenu(depth, index);
                        } else if self.menu.as_ref().is_some_and(|m| m.levels.len() > depth + 1) {
                            self.truncate_menu(depth);
                        }
                        self.redraw_level(depth);
                    }
                } else if let Some((depth, None)) = hit {
                    let _ = depth;
                } else if let Some(menu) = self.menu.as_mut() {
                    let last = menu.levels.len() - 1;
                    if menu.levels[last].selected.is_some() && menu.levels[last].open.is_none() {
                        menu.levels[last].selected = None;
                        self.redraw_level(last);
                    }
                }
                Verdict::Consumed
            }
            PointerKind::Button {
                button,
                pressed: true,
            } => {
                match hit {
                    Some((depth, Some(index))) => {
                        self.press = Some(Press {
                            button: button_number(button),
                            context: Context::Root,
                            window: None,
                            at: (event.x, event.y),
                            dragged: false,
                            consumed: true,
                        });
                        if matches!(menu_entry(self, depth, index), Some(Entry::Submenu { .. })) {
                            self.open_submenu(depth, index);
                        }
                    }
                    Some((_, None)) => {}
                    None => {
                        self.close_menu();
                        self.press = None;
                    }
                }
                Verdict::Consumed
            }
            PointerKind::Button {
                button,
                pressed: false,
            } => {
                let number = button_number(button);
                if let Some(menu) = self.menu.as_mut()
                    && menu.opening_button == Some(number)
                {
                    menu.opening_button = None;
                    if hit.is_none_or(|(_, i)| i.is_none()) {
                        return Verdict::Consumed;
                    }
                }
                if let Some((depth, Some(index))) = hit {
                    self.activate_entry(depth, index);
                }
                self.press = None;
                Verdict::Consumed
            }
            PointerKind::Axis { .. } => Verdict::Consumed,
        }
    }

    fn menu_key(&mut self, event: &KeyEvent) {
        let Some(menu) = self.menu.as_mut() else {
            return;
        };
        let depth = menu.levels.len() - 1;
        let level = &mut menu.levels[depth];
        match event.keysym {
            keys::ESCAPE => self.close_menu(),
            keys::DOWN => {
                level.selected = level.step_selection(level.selected, 1);
                self.redraw_level(depth);
            }
            keys::UP => {
                level.selected = level.step_selection(level.selected, -1);
                self.redraw_level(depth);
            }
            keys::LEFT => {
                if depth > 0 {
                    self.truncate_menu(depth - 1);
                    self.redraw_level(depth - 1);
                }
            }
            keys::RIGHT => {
                if let Some(index) = level.selected
                    && matches!(level.entries[index], Entry::Submenu { .. })
                {
                    self.open_submenu(depth, index);
                    if let Some(menu) = self.menu.as_mut()
                        && let Some(child) = menu.levels.get_mut(depth + 1)
                    {
                        child.selected = child.step_selection(None, 1);
                    }
                    self.redraw_level(depth + 1);
                }
            }
            keys::RETURN | keys::KP_ENTER | keys::SPACE => {
                if let Some(index) = level.selected {
                    self.activate_entry(depth, index);
                }
            }
            _ => {}
        }
    }

    /// Pick up the pipe menus that answered, filling the level that waits for each.
    fn poll_pipes(&mut self) {
        if self.pipes.is_empty() {
            return;
        }
        let mut finished = Vec::new();
        for (i, job) in self.pipes.iter().enumerate() {
            if let Some(result) = job.poll() {
                finished.push((i, job.menu_id.clone(), result));
            }
        }
        for (i, menu_id, result) in finished.into_iter().rev() {
            self.pipes.remove(i);
            match result {
                Ok(text) => match self.menus.parse_pipe(&text) {
                    Ok(items) => {
                        if let Some(menu) = self.menus.menus.get_mut(&menu_id) {
                            menu.items = items;
                        }
                    }
                    Err(e) => log::warn!(target: "wm", "pipe menu {menu_id}: {e}"),
                },
                Err(e) => log::warn!(target: "wm", "pipe menu {menu_id}: {e}"),
            }
            let depth = self
                .menu
                .as_ref()
                .and_then(|m| m.levels.iter().position(|l| l.id == menu_id && l.loading));
            if let Some(depth) = depth {
                let (window, output, at) = {
                    let menu = self.menu.as_ref().unwrap();
                    let level = &menu.levels[depth];
                    (menu.window, menu.output, (level.rect.x, level.rect.y))
                };
                let area = self.workarea(output);
                let scale = self.scale_of(output);
                let (entries, loading) = self.entries_for(&menu_id, window);
                let key = MENU_KEY + depth as u32;
                let mut level =
                    Level::new(&mut self.renderer, menu_id.clone(), entries, at, area, scale, key);
                level.loading = loading;
                let look = self.theme.look;
                if let Some(image) = level.render(&mut self.renderer, &look, scale) {
                    self.out.push(Command::Overlay {
                        key,
                        rect: level.rect,
                        image,
                    });
                }
                if let Some(menu) = self.menu.as_mut() {
                    menu.levels.truncate(depth);
                    menu.levels.push(level);
                }
            }
        }
        if !self.pipes.is_empty() {
            self.out
                .push(Command::WakeAt(self.now + Duration::from_millis(50)));
        }
    }

    // ---- ticks -----------------------------------------------------------------------------------

    fn tick(&mut self, now: Instant) {
        self.now = now;
        let (frames, done) = self.effects.tick(now);
        for (id, transform) in frames {
            self.out.push(Command::Effect {
                id,
                transform: Some(transform),
            });
        }
        for (id, kind) in done {
            let shaded = self.win(id).is_some_and(|w| w.shaded);
            if !shaded {
                self.out.push(Command::Effect { id, transform: None });
            }
            if kind == EffectKind::Minimize {
                self.finish_minimize(id);
            }
        }
        if self.effects.any() {
            self.out.push(Command::WakeAt(now + pixelflux::wm::EFFECT_FRAME));
        }
        self.poll_pipes();
        if now >= self.theme_poll {
            self.theme_poll = now + Duration::from_secs(1);
            if self.theme.poll(now) {
                self.look_generation += 1;
                self.renderer.set_icon_theme(self.theme.icon_theme.as_deref());
                let ids: Vec<WindowId> = self.windows.keys().copied().collect();
                for id in ids {
                    self.apply_decoration_choice(id);
                    if self.windows[&id].maximized {
                        self.fit_maximized(id);
                    }
                    self.configure(id);
                    self.render_frame(id);
                }
                for depth in 0..self.menu.as_ref().map(|m| m.levels.len()).unwrap_or(0) {
                    self.redraw_level(depth);
                }
                self.out.push(Command::Redraw);
                log::info!(target: "wm", "desktop theme changed; frames drawn again");
            }
        }
        self.out.push(Command::WakeAt(self.theme_poll));
    }
}

/// The icon name an app id resolves to in an icon theme: the id itself, else its last segment.
fn icon_name(app_id: &str) -> String {
    app_id.to_string()
}

/// Whether a window of this type lays itself out (a dock, a desktop, a splash, a menu, a
/// notification, a tooltip, a drag icon) and so keeps the geometry it asks for, as opposed to
/// the kinds the policy places and keeps on screen.
fn places_itself(window_type: WindowType) -> bool {
    !matches!(
        window_type,
        WindowType::Normal | WindowType::Dialog | WindowType::Utility | WindowType::Toolbar
    )
}

fn win_output(windows: &HashMap<WindowId, Win>, id: WindowId) -> OutputId {
    windows.get(&id).map(|w| w.output).unwrap_or(0)
}

fn menu_entry(engine: &Engine, depth: usize, index: usize) -> Option<&Entry> {
    engine.menu.as_ref()?.levels.get(depth)?.entries.get(index)
}

impl WindowPolicy for Engine {
    fn handle(&mut self, event: Event, out: &mut Vec<Command>) -> Verdict {
        self.now = Instant::now();
        let verdict = match event {
            Event::Outputs(outputs) => {
                self.outputs_changed(outputs);
                Verdict::Pass
            }
            Event::Added(info) => {
                self.added(info);
                Verdict::Pass
            }
            Event::Changed(info) => {
                self.changed(info);
                Verdict::Pass
            }
            Event::Mapped { id, size } => {
                self.mapped(id, size);
                Verdict::Pass
            }
            Event::Resized { id, size } => {
                self.resized(id, size);
                Verdict::Pass
            }
            Event::Unmapped { id } => {
                self.unmapped(id);
                Verdict::Pass
            }
            Event::Removed { id } => {
                self.removed(id);
                Verdict::Pass
            }
            Event::Request { id, request } => {
                self.request(id, request);
                Verdict::Pass
            }
            Event::TaskbarRect { id, rect } => {
                if let Some(win) = self.windows.get_mut(&id) {
                    win.taskbar = rect;
                }
                Verdict::Pass
            }
            Event::ShowDesktop(on) => {
                self.show_desktop(on);
                Verdict::Pass
            }
            Event::Pointer(event, under) => self.pointer(event, under),
            Event::Key(event) => self.key(event),
            Event::Tick(now) => {
                self.tick(now);
                Verdict::Pass
            }
            Event::X11Display(display) => {
                self.launcher.set_x11_display(display);
                Verdict::Pass
            }
        };
        if !self.out.iter().any(|c| matches!(c, Command::WakeAt(_))) {
            self.out.push(Command::WakeAt(self.theme_poll));
        }
        out.append(&mut self.out);
        verdict
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct Recorder(Arc<Mutex<Vec<String>>>);

    impl Launcher for Recorder {
        fn spawn(&mut self, command: &str) {
            self.0.lock().unwrap().push(command.to_string());
        }
        fn env(&self) -> Vec<(String, String)> {
            Vec::new()
        }
    }

    fn engine() -> (Engine, Arc<Mutex<Vec<String>>>) {
        let spawned = Arc::new(Mutex::new(Vec::new()));
        let mut engine = Engine::new(Backend::Wayland, Box::new(Recorder(spawned.clone())));
        engine.config = Config::default();
        engine.menus = Menus::default();
        let mut out = Vec::new();
        engine.handle(
            Event::Outputs(vec![OutputInfo {
                id: 0,
                rect: Rect::new(0, 0, 1280, 800),
                workarea: Rect::new(0, 0, 1280, 768),
                scale: 1.0,
                primary: true,
            }]),
            &mut out,
        );
        (engine, spawned)
    }

    fn info(id: WindowId) -> WindowInfo {
        WindowInfo {
            id,
            kind: Some(pixelflux::wm::WindowKind::Xdg),
            title: format!("Window {id}"),
            app_id: "foot".to_string(),
            accepts_focus: true,
            ..WindowInfo::default()
        }
    }

    fn open(engine: &mut Engine, id: WindowId, w: i32, h: i32) -> Vec<Command> {
        let mut out = Vec::new();
        engine.handle(Event::Added(info(id)), &mut out);
        engine.handle(
            Event::Mapped {
                id,
                size: Size { w, h },
            },
            &mut out,
        );
        out
    }

    /// A new window is placed in the workarea below its frame, framed, focused, and stacked on
    /// top; a second one lands beside it.
    #[test]
    fn a_new_window_is_placed_framed_and_focused() {
        let (mut engine, _) = engine();
        let out = open(&mut engine, 1, 600, 400);
        let configure = out.iter().rev().find_map(|c| match c {
            Command::Configure { id: 1, rect, states } => Some((*rect, *states)),
            _ => None,
        });
        let (rect, states) = configure.unwrap();
        assert!(rect.y >= frame::TITLE_HEIGHT && rect.x >= frame::BORDER);
        assert!(states.activated);
        assert!(out.iter().any(
            |c| matches!(c, Command::Frame { id: 1, frame: Some(f) } if f.extents.top == frame::TITLE_HEIGHT)
        ));
        assert!(out.iter().any(|c| matches!(c, Command::Focus(Some(1)))));
        assert!(
            out.iter()
                .any(|c| matches!(c, Command::Stack(s) if s == &vec![1]))
        );
        let out = open(&mut engine, 2, 300, 200);
        let second = out
            .iter()
            .rev()
            .find_map(|c| match c {
                Command::Configure { id: 2, rect, .. } => Some(*rect),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            second
                .grown(engine.extents_of(2))
                .intersection(&rect.grown(engine.extents_of(1)))
                .area(),
            0
        );
        assert_eq!(engine.focus, Some(2));
        assert!(
            out.iter()
                .any(|c| matches!(c, Command::Stack(s) if s == &vec![1, 2]))
        );
    }

    /// A click on the title bar focuses and raises the window and is not passed on; a drag on it
    /// moves the window; a click on the close button asks it to close.
    #[test]
    fn the_title_bar_takes_clicks_and_drags() {
        let (mut engine, _) = engine();
        open(&mut engine, 1, 600, 400);
        open(&mut engine, 2, 300, 200);
        let rect1 = engine.windows[&1].rect;
        let mut out = Vec::new();
        let title = (rect1.x as f64 + 200.0, rect1.y as f64 - 10.0);
        let press = |x: f64, y: f64, pressed: bool| {
            Event::Pointer(
                PointerEvent {
                    x,
                    y,
                    kind: PointerKind::Button {
                        button: BTN_LEFT,
                        pressed,
                    },
                    modifiers: Modifiers::default(),
                    time: 1000,
                },
                Under::default(),
            )
        };
        let motion = |x: f64, y: f64| {
            Event::Pointer(
                PointerEvent {
                    x,
                    y,
                    kind: PointerKind::Motion,
                    modifiers: Modifiers::default(),
                    time: 1001,
                },
                Under::default(),
            )
        };
        assert_eq!(
            engine.handle(motion(title.0, title.1), &mut out),
            Verdict::Consumed
        );
        assert_eq!(
            engine.handle(press(title.0, title.1, true), &mut out),
            Verdict::Consumed
        );
        assert_eq!(engine.focus, Some(1));
        assert!(
            out.iter()
                .any(|c| matches!(c, Command::Stack(s) if s == &vec![2, 1]))
        );
        assert_eq!(
            engine.handle(motion(title.0 + 50.0, title.1 + 30.0), &mut out),
            Verdict::Consumed
        );
        assert_eq!(
            engine.handle(press(title.0 + 50.0, title.1 + 30.0, false), &mut out),
            Verdict::Consumed
        );
        let moved = engine.windows[&1].rect;
        assert_eq!((moved.x, moved.y), (rect1.x + 50, rect1.y + 30));
        let layout = frame::layout(&engine.theme.look.style, Size { w: 600, h: 400 }, false, false);
        let (_, close) = layout.buttons.last().copied().unwrap();
        let frame = engine.frame_rect(1);
        let at = ((frame.x + close.x + 5) as f64, (frame.y + close.y + 5) as f64);
        out.clear();
        engine.handle(press(at.0, at.1, true), &mut out);
        engine.handle(press(at.0, at.1, false), &mut out);
        assert!(out.iter().any(|c| matches!(c, Command::Close(1))));
    }

    /// A click inside a client focuses it and passes through; Alt-F4 is taken and closes the
    /// focused window; a minimize hides the window after its effect and the focus falls back.
    #[test]
    fn clients_keys_and_minimize() {
        let (mut engine, _) = engine();
        open(&mut engine, 1, 600, 400);
        open(&mut engine, 2, 300, 200);
        let mut out = Vec::new();
        let rect1 = engine.windows[&1].rect;
        let verdict = engine.handle(
            Event::Pointer(
                PointerEvent {
                    x: rect1.x as f64 + 10.0,
                    y: rect1.y as f64 + 10.0,
                    kind: PointerKind::Button {
                        button: BTN_LEFT,
                        pressed: true,
                    },
                    modifiers: Modifiers::default(),
                    time: 5,
                },
                Under {
                    window: Some(1),
                    top_layer: false,
                    desktop: false,
                },
            ),
            &mut out,
        );
        assert_eq!(verdict, Verdict::Pass);
        assert_eq!(engine.focus, Some(1));
        let f4 = KeyEvent {
            keysym: 0xffc1,
            base_keysym: 0xffc1,
            keycode: 70,
            pressed: true,
            modifiers: Modifiers {
                alt: true,
                ..Modifiers::default()
            },
        };
        out.clear();
        assert_eq!(engine.handle(Event::Key(f4), &mut out), Verdict::Consumed);
        assert!(out.iter().any(|c| matches!(c, Command::Close(1))));
        assert_eq!(
            engine.handle(Event::Key(KeyEvent { pressed: false, ..f4 }), &mut out),
            Verdict::Consumed
        );
        out.clear();
        engine.handle(
            Event::Request {
                id: 1,
                request: Request::Minimize,
            },
            &mut out,
        );
        assert_eq!(engine.focus, Some(2));
        assert!(out.iter().any(|c| matches!(c, Command::WakeAt(_))));
        engine.handle(Event::Tick(Instant::now() + Duration::from_secs(1)), &mut out);
        assert!(engine.windows[&1].minimized);
        assert!(
            out.iter()
                .any(|c| matches!(c, Command::Configure { id: 1, states, .. } if states.minimized))
        );
        assert!(
            out.iter()
                .any(|c| matches!(c, Command::Stack(s) if s == &vec![2]))
        );
        out.clear();
        engine.handle(
            Event::Request {
                id: 1,
                request: Request::Activate,
            },
            &mut out,
        );
        assert!(!engine.windows[&1].minimized);
        assert_eq!(engine.focus, Some(1));
    }

    /// Maximize fills the workarea under the title bar and restore brings the geometry back;
    /// the root menu opens on a right click of the bare screen and runs an entry's command.
    #[test]
    fn maximize_and_the_root_menu() {
        let (mut engine, spawned) = engine();
        engine
            .menus
            .apply(r#"<openbox_menu><menu id="root-menu"><item label="Terminal"><action name="Execute"><command>xterm</command></action></item></menu></openbox_menu>"#)
            .unwrap();
        open(&mut engine, 1, 600, 400);
        let before = engine.windows[&1].rect;
        let mut out = Vec::new();
        engine.handle(
            Event::Request {
                id: 1,
                request: Request::Maximize,
            },
            &mut out,
        );
        let rect = engine.windows[&1].rect;
        assert_eq!(
            rect,
            Rect::new(0, frame::TITLE_HEIGHT, 1280, 768 - frame::TITLE_HEIGHT)
        );
        engine.handle(
            Event::Request {
                id: 1,
                request: Request::Unmaximize,
            },
            &mut out,
        );
        assert_eq!(engine.windows[&1].rect, before);
        out.clear();
        let right = |pressed: bool| {
            Event::Pointer(
                PointerEvent {
                    x: 1200.0,
                    y: 700.0,
                    kind: PointerKind::Button {
                        button: BTN_RIGHT,
                        pressed,
                    },
                    modifiers: Modifiers::default(),
                    time: 9,
                },
                Under::default(),
            )
        };
        assert_eq!(engine.handle(right(true), &mut out), Verdict::Consumed);
        let overlay = out
            .iter()
            .find_map(|c| match c {
                Command::Overlay { key, rect, .. } if *key == MENU_KEY => Some(*rect),
                _ => None,
            })
            .unwrap();
        assert!(overlay.right() <= 1280 && overlay.bottom() <= 768);
        engine.handle(right(false), &mut out);
        assert!(engine.menu.is_some());
        let row = engine.menu.as_ref().unwrap().levels[0].row_rect(0);
        let click = |pressed: bool| {
            Event::Pointer(
                PointerEvent {
                    x: row.x as f64 + 20.0,
                    y: row.y as f64 + 5.0,
                    kind: PointerKind::Button {
                        button: BTN_LEFT,
                        pressed,
                    },
                    modifiers: Modifiers::default(),
                    time: 20,
                },
                Under::default(),
            )
        };
        engine.handle(click(true), &mut out);
        engine.handle(click(false), &mut out);
        assert!(engine.menu.is_none());
        assert_eq!(spawned.lock().unwrap().as_slice(), ["xterm"]);
        assert!(
            out.iter()
                .any(|c| matches!(c, Command::ClearOverlay(k) if *k == MENU_KEY))
        );
    }
}
