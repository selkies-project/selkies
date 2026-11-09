/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The openbox-format configuration the images ship, read as the subset they set: `rc.xml` for
//! focus, placement, resistance, the decoration default, the keyboard and mouse bindings over the
//! built-in defaults, the `<applications>` rules, and the menu file; `menu.xml` and the pipe menus
//! it names for the root menu.
//!
//! The files are looked for where openbox looks, `openbox/` under the user's configuration
//! directory first and then under the system ones, on both backends alike, so one set of files
//! configures the desktop whichever backend runs it. labwc's dialect of the format is read too
//! (a `<labwc_config>` root, `onRelease`, an action's `command` attribute, `<core><decoration>`),
//! so a configuration written for it carries over. What neither dialect's users configure here
//! (the full actions language, several desktops, chords) is not implemented; an action of a name
//! this module does not know parses as [`Action::Unknown`] and is logged once when it would run.
//!
//! The defaults are openbox's own bindings, the ones its stock `rc.xml` carries, with the
//! Super-arrow quick tiles (`SnapToEdge`) in place of its edge moves, so a `<keyboard>` or
//! `<mouse>` section that names only its additions, as labwc's `<default />` does, keeps the
//! rest; an `rc.xml` that lists a full mouse section replaces the defaults for every context it
//! names, as openbox reads it.

pub mod xml;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use pixelflux::wm::{Modifiers, ResizeEdge};

use self::xml::{Node, truthy};

/// Which display server the configuration is read for; it decides which window manager's
/// directory is searched first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Wayland,
    X11,
}

/// Where new windows go.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PlacementPolicy {
    /// The free spot overlapping the least, as openbox's `Smart`.
    #[default]
    Smart,
    /// Each new window a step down and right of the last.
    Cascade,
    /// Centered on the output.
    Center,
    /// Under the pointer.
    UnderMouse,
}

/// Which output a new window opens on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PlacementMonitor {
    /// The one under the pointer.
    #[default]
    Mouse,
    /// The one holding the focused window.
    Active,
    /// The primary.
    Primary,
}

/// A direction a maximize applies to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Direction {
    #[default]
    Both,
    Horizontal,
    Vertical,
}

/// One action a binding or a menu item runs.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Execute(String),
    ShowMenu(String),
    Close,
    Kill,
    Iconify,
    Focus,
    Unfocus,
    Raise,
    Lower,
    RaiseLower,
    FocusToBottom,
    Maximize(Direction),
    Unmaximize(Direction),
    ToggleMaximize(Direction),
    Fullscreen,
    Unfullscreen,
    ToggleFullscreen,
    Shade,
    Unshade,
    ToggleShade,
    Decorate,
    Undecorate,
    ToggleDecorations,
    ToggleAlwaysOnTop,
    ToggleAlwaysOnBottom,
    Move,
    Resize(Option<ResizeEdge>),
    MoveResizeTo {
        x: Option<Coordinate>,
        y: Option<Coordinate>,
        width: Option<i32>,
        height: Option<i32>,
    },
    MoveToEdge(ResizeEdge),
    /// Quick-tile the window toward an edge of its output: a half, then a quarter, or back
    /// (`SnapToEdge` in labwc's spelling; the engine documents the transitions).
    SnapToEdge(ResizeEdge),
    /// Switch to a virtual desktop.
    GoToDesktop(DesktopTarget),
    /// Move the window to a virtual desktop, switching there too when `follow` is set.
    SendToDesktop {
        to: DesktopTarget,
        follow: bool,
    },
    /// Put the window on every desktop, or back on the current one alone.
    ToggleOmnipresent,
    NextWindow,
    PreviousWindow,
    ShowDesktop,
    ToggleShowDesktop,
    Reconfigure,
    /// An action this manager does not implement, kept by name for the log.
    Unknown(String),
}

/// The desktop a `GoToDesktop` or `SendToDesktop` names in its `<to>` element.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DesktopTarget {
    /// A desktop by number, counted from zero here and from one in the file.
    Index(u32),
    /// The next desktop, wrapping to the first (`next`, `right`, `down`).
    Next,
    /// The desktop before, wrapping to the last (`previous`, `left`, `up`).
    Previous,
    Last,
}

impl DesktopTarget {
    /// The target openbox's spelling names; `None` for an unknown word or a number below one.
    pub fn parse(text: &str) -> Option<DesktopTarget> {
        let text = text.trim().to_ascii_lowercase();
        Some(match text.as_str() {
            "next" | "right" | "east" | "down" | "south" => DesktopTarget::Next,
            "previous" | "prev" | "left" | "west" | "up" | "north" => DesktopTarget::Previous,
            "last" => DesktopTarget::Last,
            number => DesktopTarget::Index(number.parse::<u32>().ok().filter(|n| *n >= 1)? - 1),
        })
    }
}

/// A position in an `<x>` or `<y>` element: a number, one from the far edge, or centered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Coordinate {
    Center,
    FromStart(i32),
    FromEnd(i32),
}

impl Coordinate {
    /// The coordinate openbox's spelling names: `center`, a number, or a negative number from
    /// the far edge.
    pub fn parse(text: &str) -> Option<Coordinate> {
        let text = text.trim();
        if text.eq_ignore_ascii_case("center") {
            return Some(Coordinate::Center);
        }
        if let Some(rest) = text.strip_prefix('-') {
            return rest.parse().ok().map(Coordinate::FromEnd);
        }
        text.parse().ok().map(Coordinate::FromStart)
    }

    /// The value along an axis of `size` for a window of `extent`, from `origin`.
    pub fn resolve(&self, origin: i32, size: i32, extent: i32) -> i32 {
        match self {
            Coordinate::Center => origin + (size - extent) / 2,
            Coordinate::FromStart(v) => origin + v,
            Coordinate::FromEnd(v) => origin + size - extent - v,
        }
    }
}

/// A key chord: the modifiers held and the keysym, matched case aside.
#[derive(Clone, Debug, PartialEq)]
pub struct KeyBind {
    pub modifiers: Modifiers,
    pub keysym: u32,
    /// Whether the actions run on the key's release rather than its press.
    pub on_release: bool,
    pub actions: Vec<Action>,
    /// The chord as the configuration spelled it, canonically ([`chord_spec`]), before the
    /// user's overrides moved it; the key an override names the bind by. Empty for a bind the
    /// user added.
    pub origin: String,
    /// Whether an override took the bind out of service: it is listed, and matches nothing.
    pub disabled: bool,
}

impl KeyBind {
    /// A bind on a chord, its origin that chord.
    pub fn new(modifiers: Modifiers, keysym: u32, on_release: bool, actions: Vec<Action>) -> KeyBind {
        KeyBind {
            modifiers,
            keysym,
            on_release,
            actions,
            origin: chord_spec(modifiers, keysym),
            disabled: false,
        }
    }

    /// The chord in force, canonically spelled; empty for a disabled bind.
    pub fn spec(&self) -> String {
        if self.disabled {
            String::new()
        } else {
            chord_spec(self.modifiers, self.keysym)
        }
    }

    /// Whether the bind is the user's own, added by an `execute` override.
    pub fn custom(&self) -> bool {
        self.origin.is_empty()
    }
}

/// What a pointer did on a binding's context.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseEvent {
    Press,
    Release,
    Click,
    DoubleClick,
    Drag,
}

/// Where on a window or the screen a pointer binding applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Context {
    /// Anywhere on a window, frame or client.
    Frame,
    Titlebar,
    Top,
    Bottom,
    Left,
    Right,
    TLCorner,
    TRCorner,
    BLCorner,
    BRCorner,
    /// The client area.
    Client,
    /// The window icon on the title bar.
    Icon,
    /// The title bar buttons.
    Iconify,
    Maximize,
    Close,
    Shade,
    AllDesktops,
    /// The bare screen.
    Root,
    /// A desktop window (a shell's wallpaper).
    Desktop,
    /// Any part while a move or resize runs.
    MoveResize,
}

impl Context {
    /// The context openbox names, case aside.
    pub fn parse(name: &str) -> Option<Context> {
        Some(match name.trim().to_ascii_lowercase().as_str() {
            "frame" => Context::Frame,
            "titlebar" => Context::Titlebar,
            "top" => Context::Top,
            "bottom" | "handle" => Context::Bottom,
            "left" => Context::Left,
            "right" => Context::Right,
            "tlcorner" => Context::TLCorner,
            "trcorner" => Context::TRCorner,
            "blcorner" => Context::BLCorner,
            "brcorner" => Context::BRCorner,
            "client" => Context::Client,
            "icon" => Context::Icon,
            "iconify" => Context::Iconify,
            "maximize" => Context::Maximize,
            "close" => Context::Close,
            "shade" => Context::Shade,
            "alldesktops" => Context::AllDesktops,
            "root" => Context::Root,
            "desktop" => Context::Desktop,
            "moveresize" => Context::MoveResize,
            _ => return None,
        })
    }
}

/// One pointer binding.
#[derive(Clone, Debug, PartialEq)]
pub struct MouseBind {
    pub context: Context,
    pub modifiers: Modifiers,
    /// The button number openbox counts, 1 for the left button.
    pub button: u32,
    pub event: MouseEvent,
    pub actions: Vec<Action>,
}

/// A window layer a rule names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleLayer {
    Above,
    Normal,
    Below,
}

/// What a rule applies to a window matching it; a `None` leaves the window as it would be.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Rule {
    /// The X11 instance name, or the app id, as a glob.
    pub name: Option<String>,
    /// The X11 class, or the app id, as a glob.
    pub class: Option<String>,
    pub title: Option<String>,
    pub window_type: Option<String>,
    pub decor: Option<bool>,
    pub position: Option<(Coordinate, Coordinate)>,
    pub position_forced: bool,
    pub size: Option<(i32, i32)>,
    pub monitor: Option<u32>,
    pub focus: Option<bool>,
    pub layer: Option<RuleLayer>,
    pub iconic: Option<bool>,
    pub skip_taskbar: Option<bool>,
    pub fullscreen: Option<bool>,
    pub maximized: Option<bool>,
    pub shade: Option<bool>,
}

impl Rule {
    /// Whether the rule's patterns all match a window's names.
    pub fn matches(&self, instance: &str, class: &str, title: &str, window_type: &str) -> bool {
        let glob = |pattern: &Option<String>, value: &str| match pattern {
            None => true,
            Some(p) => glob_match(p, value),
        };
        glob(&self.name, instance) && glob(&self.class, class) && glob(&self.title, title) && {
            match &self.window_type {
                None => true,
                Some(t) => t.eq_ignore_ascii_case(window_type),
            }
        }
    }
}

/// Whether `pattern`, with `*` for any run and `?` for one character, matches `value` in full,
/// case aside.
pub fn glob_match(pattern: &str, value: &str) -> bool {
    fn step(p: &[char], v: &[char]) -> bool {
        match (p.first(), v.first()) {
            (None, None) => true,
            (Some('*'), _) => step(&p[1..], v) || (!v.is_empty() && step(p, &v[1..])),
            (Some('?'), Some(_)) => step(&p[1..], &v[1..]),
            (Some(a), Some(b)) if a.eq_ignore_ascii_case(b) => step(&p[1..], &v[1..]),
            _ => false,
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let v: Vec<char> = value.chars().collect();
    step(&p, &v)
}

/// What `rc.xml` sets.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub focus_new: bool,
    pub follow_mouse: bool,
    pub raise_on_focus: bool,
    pub placement: PlacementPolicy,
    pub placement_center: bool,
    pub placement_monitor: PlacementMonitor,
    /// Pixels a moved window resists another window's edge.
    pub resistance: i32,
    /// Pixels a moved window resists the screen's edge.
    pub screen_edge_resistance: i32,
    /// Whether a client that leaves the choice open is framed by the server.
    pub decorate_by_default: bool,
    pub double_click_ms: u32,
    pub drag_threshold: i32,
    pub keybinds: Vec<KeyBind>,
    pub mousebinds: Vec<MouseBind>,
    /// The menu files `<menu><file>` names, `menu.xml` when none.
    pub menu_files: Vec<String>,
    pub show_menu_icons: bool,
    pub rules: Vec<Rule>,
    /// The number of virtual desktops `<desktops><number>` names; the desktop shell's own
    /// `desktops` setting wins over it.
    pub desktops: Option<u32>,
    /// The `<theme><name>` the file names, kept for the log.
    pub theme_name: Option<String>,
    /// The files the configuration was read from, for the startup line.
    pub sources: Vec<PathBuf>,
    /// The directories the configuration was searched in, in order, for the menu files.
    pub search_dirs: Vec<PathBuf>,
    /// When the user's key bind overrides file was last modified as read, so a later change
    /// to it is noticed.
    pub overrides_seen: Option<std::time::SystemTime>,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            focus_new: true,
            follow_mouse: false,
            raise_on_focus: false,
            placement: PlacementPolicy::Smart,
            placement_center: true,
            placement_monitor: PlacementMonitor::Mouse,
            resistance: 10,
            screen_edge_resistance: 20,
            decorate_by_default: true,
            double_click_ms: 500,
            drag_threshold: 8,
            keybinds: default_keybinds(),
            mousebinds: default_mousebinds(),
            menu_files: vec!["menu.xml".to_string()],
            show_menu_icons: true,
            rules: Vec::new(),
            desktops: None,
            theme_name: None,
            sources: Vec::new(),
            search_dirs: Vec::new(),
            overrides_seen: None,
        }
    }
}

/// The directories an `rc.xml` or a `menu.xml` is looked for in, in precedence order: the
/// user's configuration directory and then the system ones, each under the backend's own
/// window manager's name first and the other's second.
pub fn search_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![selkies_draw::dirs::config_home().join("openbox")];
    dirs.extend(
        selkies_draw::dirs::config_dirs()
            .into_iter()
            .map(|dir| dir.join("openbox")),
    );
    dirs
}

/// The first existing file of this name along the search directories.
pub fn find_file(dirs: &[PathBuf], name: &str) -> Option<PathBuf> {
    let name = Path::new(name);
    if name.is_absolute() {
        return name.is_file().then(|| name.to_path_buf());
    }
    dirs.iter().map(|d| d.join(name)).find(|p| p.is_file())
}

/// The file the user's key bind overrides live in: `selkies-wm/keybinds` under the
/// configuration home, which the desktop shell's shortcut editor writes.
pub fn overrides_path() -> PathBuf {
    selkies_draw::dirs::config_home().join("selkies-wm/keybinds")
}

/// When a file was last modified, `None` where it cannot be read.
pub fn modified(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

impl Config {
    /// The configuration: the first `rc.xml` found along [`search_dirs`] over the defaults, or
    /// the defaults alone, with the user's key bind overrides applied over either.
    pub fn load() -> Config {
        let dirs = search_dirs();
        let mut config = Config::default();
        if let Some(path) = find_file(&dirs, "rc.xml") {
            match std::fs::read_to_string(&path) {
                Ok(text) => match config.apply_rc(&text) {
                    Ok(()) => config.sources.push(path),
                    Err(e) => log::warn!(target: "wm", "{}: not read: {e}", path.display()),
                },
                Err(e) => log::warn!(target: "wm", "{}: not read: {e}", path.display()),
            }
        }
        config.search_dirs = dirs;
        let overrides = overrides_path();
        config.overrides_seen = modified(&overrides);
        if let Ok(text) = std::fs::read_to_string(&overrides) {
            let applied = config.apply_overrides(&text);
            if applied > 0 {
                config.sources.push(overrides);
            }
        }
        config
    }

    /// Whether the overrides file changed since the configuration was read.
    pub fn overrides_changed(&self) -> bool {
        modified(&overrides_path()) != self.overrides_seen
    }

    /// Fold the user's key bind overrides into the binds, one per line, tab-separated:
    /// `rebind`, the origin chord, and the chord that replaces it (empty to disable the bind);
    /// or `execute`, a chord, and the command a new bind runs. A `#` line or a blank one is
    /// skipped, as is a line naming a chord that does not parse or an origin no bind has.
    ///
    /// # Returns
    /// How many lines took effect.
    pub fn apply_overrides(&mut self, text: &str) -> usize {
        let mut applied = 0;
        for line in text.lines() {
            let line = line.trim_end_matches('\r');
            if line.trim().is_empty() || line.trim_start().starts_with('#') {
                continue;
            }
            let fields: Vec<&str> = line.split('\t').collect();
            match fields.as_slice() {
                ["rebind", origin, to] => {
                    let origin = match canonical_spec(origin) {
                        Some(spec) => spec,
                        None => continue,
                    };
                    let to = if to.trim().is_empty() {
                        None
                    } else {
                        match parse_spec(to) {
                            Some(chord) => Some(chord),
                            None => {
                                log::debug!(target: "wm", "keybind override {to:?}: unknown chord");
                                continue;
                            }
                        }
                    };
                    let mut found = false;
                    for bind in self.keybinds.iter_mut().filter(|b| b.origin == origin) {
                        found = true;
                        match to {
                            Some((modifiers, keysym)) => {
                                bind.modifiers = modifiers;
                                bind.keysym = keysym;
                                bind.disabled = false;
                            }
                            None => bind.disabled = true,
                        }
                    }
                    if found {
                        applied += 1;
                    }
                }
                ["execute", spec, command] if !command.trim().is_empty() => {
                    let Some((modifiers, keysym)) = parse_spec(spec) else {
                        log::debug!(target: "wm", "keybind override {spec:?}: unknown chord");
                        continue;
                    };
                    self.keybinds.push(KeyBind {
                        modifiers,
                        keysym,
                        on_release: false,
                        actions: vec![Action::Execute(command.trim().to_string())],
                        origin: String::new(),
                        disabled: false,
                    });
                    applied += 1;
                }
                _ => {}
            }
        }
        applied
    }

    /// Fold an `rc.xml` document into the configuration.
    pub fn apply_rc(&mut self, text: &str) -> Result<(), String> {
        let root = xml::parse(text)?;
        if let Some(focus) = root.child("focus") {
            if let Some(v) = focus.yes("focusNew") {
                self.focus_new = v;
            }
            if let Some(v) = focus.yes("followMouse") {
                self.follow_mouse = v;
            }
            if let Some(v) = focus.yes("raiseOnFocus") {
                self.raise_on_focus = v;
            }
        }
        if let Some(placement) = root.child("placement") {
            if let Some(policy) = placement.child_text("policy") {
                self.placement = match policy.to_ascii_lowercase().as_str() {
                    "cascade" => PlacementPolicy::Cascade,
                    "center" | "centre" => PlacementPolicy::Center,
                    "undermouse" => PlacementPolicy::UnderMouse,
                    _ => PlacementPolicy::Smart,
                };
            }
            if let Some(v) = placement.yes("center") {
                self.placement_center = v;
            }
            if let Some(monitor) = placement.child_text("monitor") {
                self.placement_monitor = match monitor.to_ascii_lowercase().as_str() {
                    "active" => PlacementMonitor::Active,
                    "primary" => PlacementMonitor::Primary,
                    _ => PlacementMonitor::Mouse,
                };
            }
        }
        if let Some(desktops) = root.child("desktops")
            && let Some(number) = desktops.child_text("number").and_then(|t| t.parse::<u32>().ok())
            && number >= 1
        {
            self.desktops = Some(number);
        }
        if let Some(resistance) = root.child("resistance") {
            if let Some(v) = resistance.child_text("strength").and_then(|t| t.parse().ok()) {
                self.resistance = v;
            }
            if let Some(v) = resistance
                .child_text("screen_edge_strength")
                .and_then(|t| t.parse().ok())
            {
                self.screen_edge_resistance = v;
            }
        }
        if let Some(core) = root.child("core")
            && let Some(decoration) = core.child_text("decoration")
        {
            self.decorate_by_default = !decoration.eq_ignore_ascii_case("client");
        }
        if let Some(theme) = root.child("theme")
            && let Some(name) = theme.child_text("name")
        {
            self.theme_name = Some(name.to_string());
        }
        if let Some(keyboard) = root.child("keyboard") {
            let explicit: Vec<KeyBind> = keyboard
                .children_named("keybind")
                .filter_map(parse_keybind)
                .collect();
            let keep_defaults = keyboard.child("default").is_some();
            if keep_defaults || explicit.is_empty() {
                for bind in explicit {
                    self.keybinds
                        .retain(|b| !(b.modifiers == bind.modifiers && b.keysym == bind.keysym));
                    self.keybinds.push(bind);
                }
            } else {
                self.keybinds = explicit;
            }
        }
        if let Some(mouse) = root.child("mouse") {
            if let Some(v) = mouse.child_text("doubleClickTime").and_then(|t| t.parse().ok()) {
                self.double_click_ms = v;
            }
            if let Some(v) = mouse.child_text("dragThreshold").and_then(|t| t.parse().ok()) {
                self.drag_threshold = v;
            }
            let mut explicit: Vec<MouseBind> = Vec::new();
            for context in mouse.children_named("context") {
                let Some(names) = context.attr("name") else {
                    continue;
                };
                for name in names.split_whitespace() {
                    let Some(ctx) = Context::parse(name) else {
                        continue;
                    };
                    for bind in context.children_named("mousebind") {
                        if let Some(bind) = parse_mousebind(ctx, bind) {
                            explicit.push(bind);
                        }
                    }
                }
            }
            if mouse.child("default").is_some() {
                for bind in explicit {
                    self.mousebinds.retain(|b| {
                        !(b.context == bind.context
                            && b.modifiers == bind.modifiers
                            && b.button == bind.button
                            && b.event == bind.event)
                    });
                    if !bind.actions.is_empty() {
                        self.mousebinds.push(bind);
                    }
                }
            } else if !explicit.is_empty() {
                let contexts: Vec<Context> = explicit.iter().map(|b| b.context).collect();
                self.mousebinds.retain(|b| !contexts.contains(&b.context));
                self.mousebinds
                    .extend(explicit.into_iter().filter(|b| !b.actions.is_empty()));
            }
        }
        if let Some(menu) = root.child("menu") {
            let files: Vec<String> = menu
                .children_named("file")
                .map(|f| f.text.clone())
                .filter(|f| !f.is_empty())
                .collect();
            if !files.is_empty() {
                self.menu_files = files;
            }
            if let Some(v) = menu.yes("showIcons") {
                self.show_menu_icons = v;
            }
        }
        if let Some(applications) = root.child("applications") {
            for application in applications.children_named("application") {
                self.rules.push(parse_rule(application));
            }
        }
        if let Some(windows) = root.child("windowRules") {
            for rule in windows.children_named("windowRule") {
                self.rules.push(parse_rule(rule));
            }
        }
        Ok(())
    }
}

/// The modifiers and the key or button a binding's spelling names (`C-A-t`, `W-Left`): the
/// prefixes openbox and labwc take, the last token the key or button itself.
fn parse_chord(spec: &str) -> Option<(Modifiers, &str)> {
    let mut modifiers = Modifiers::default();
    let mut parts: Vec<&str> = spec.trim().split('-').collect();
    if spec.trim().ends_with('-') && parts.len() >= 2 {
        parts.pop();
        parts.pop();
        parts.push("minus");
    }
    let key = parts.pop()?;
    for part in parts {
        match part {
            "S" | "Shift" => modifiers.shift = true,
            "C" | "Control" | "Ctrl" => modifiers.ctrl = true,
            "A" | "Alt" | "Mod1" => modifiers.alt = true,
            "W" | "Super" | "Mod4" | "M" | "Meta" | "H" | "Hyper" => modifiers.logo = true,
            _ => return None,
        }
    }
    Some((modifiers, key))
}

/// The modifiers and keysym a chord's spelling names, or `None` for an unknown key.
pub fn parse_spec(spec: &str) -> Option<(Modifiers, u32)> {
    let (modifiers, name) = parse_chord(spec)?;
    Some((modifiers, keysym_from_name(name)?))
}

/// A chord's spelling in canonical form, so two spellings of one chord compare equal.
pub fn canonical_spec(spec: &str) -> Option<String> {
    parse_spec(spec).map(|(modifiers, keysym)| chord_spec(modifiers, keysym))
}

/// The canonical spelling of a chord, as a configuration names it: the modifier prefixes `C-`,
/// `A-`, `S-`, and `W-` in that order, then the keysym's name.
pub fn chord_spec(modifiers: Modifiers, keysym: u32) -> String {
    let mut spec = String::new();
    if modifiers.ctrl {
        spec.push_str("C-");
    }
    if modifiers.alt {
        spec.push_str("A-");
    }
    if modifiers.shift {
        spec.push_str("S-");
    }
    if modifiers.logo {
        spec.push_str("W-");
    }
    spec.push_str(&xkbcommon::xkb::keysym_get_name(xkbcommon::xkb::Keysym::new(
        keysym,
    )));
    spec
}

/// The keysym a bind names, the lowercase form for a letter so `t` and `T` are one key.
pub fn keysym_from_name(name: &str) -> Option<u32> {
    let sym = xkbcommon::xkb::keysym_from_name(name, xkbcommon::xkb::KEYSYM_NO_FLAGS);
    let raw = sym.raw();
    if raw == 0 {
        return None;
    }
    Some(lower_keysym(raw))
}

/// The name a chord is shown under: the modifiers as Ctrl, Alt, Shift, and Super in that order,
/// then the key, joined with `+` (`Ctrl+Alt+T`, `Super+Left`).
pub fn chord_label(modifiers: Modifiers, keysym: u32) -> String {
    let mut parts: Vec<String> = Vec::new();
    if modifiers.ctrl {
        parts.push("Ctrl".to_string());
    }
    if modifiers.alt {
        parts.push("Alt".to_string());
    }
    if modifiers.shift {
        parts.push("Shift".to_string());
    }
    if modifiers.logo {
        parts.push("Super".to_string());
    }
    parts.push(key_label(keysym));
    parts.join("+")
}

/// A key's name as a user reads it: a letter in upper case, a modifier without its side, and the
/// usual names for the others (`Enter`, `Space`, `Left`, `F4`).
fn key_label(keysym: u32) -> String {
    let name = xkbcommon::xkb::keysym_get_name(xkbcommon::xkb::Keysym::new(keysym));
    match name.as_str() {
        "Super_L" | "Super_R" => "Super".to_string(),
        "Alt_L" | "Alt_R" => "Alt".to_string(),
        "Control_L" | "Control_R" => "Ctrl".to_string(),
        "Shift_L" | "Shift_R" => "Shift".to_string(),
        "Return" | "KP_Enter" => "Enter".to_string(),
        "space" => "Space".to_string(),
        "Escape" => "Esc".to_string(),
        "Prior" => "Page Up".to_string(),
        "Next" => "Page Down".to_string(),
        "minus" => "-".to_string(),
        "plus" => "+".to_string(),
        other if other.chars().count() == 1 => other.to_uppercase(),
        other => other.replace('_', " "),
    }
}

impl Action {
    /// What the action does, in the session's language, for a list of the shortcuts in force.
    pub fn description(&self) -> String {
        use crate::i18n::tr;
        match self {
            Action::Execute(command) => match command.trim() {
                "selkies-desktop --launcher" => tr("Quick launcher").to_string(),
                "selkies-desktop --toggle-menu" => tr("Start menu").to_string(),
                "selkies-desktop --files" => tr("File manager").to_string(),
                "selkies-desktop --settings" => tr("Settings").to_string(),
                "x-terminal-emulator" | "selkies-terminal" => tr("Terminal").to_string(),
                other => format!("{} {other}", tr("Run command")),
            },
            Action::ShowMenu(id) if id == "client-menu" => tr("Window menu").to_string(),
            Action::ShowMenu(id) if id.starts_with("client-list") => tr("Windows").to_string(),
            Action::ShowMenu(_) => tr("Desktop menu").to_string(),
            Action::Close | Action::Kill => tr("Close the window").to_string(),
            Action::Iconify => tr("Minimize the window").to_string(),
            Action::Focus | Action::Raise => tr("Raise the window").to_string(),
            Action::Unfocus | Action::Lower | Action::RaiseLower | Action::FocusToBottom => {
                tr("Send the window to the back").to_string()
            }
            Action::Maximize(_) => tr("Maximize the window").to_string(),
            Action::Unmaximize(_) => tr("Restore the window").to_string(),
            Action::ToggleMaximize(_) => tr("Maximize or restore the window").to_string(),
            Action::Fullscreen | Action::Unfullscreen | Action::ToggleFullscreen => {
                tr("Fullscreen").to_string()
            }
            Action::Shade | Action::Unshade | Action::ToggleShade => tr("Shade the window").to_string(),
            Action::Decorate | Action::Undecorate | Action::ToggleDecorations => {
                tr("Decorations").to_string()
            }
            Action::ToggleAlwaysOnTop => tr("Always on top").to_string(),
            Action::ToggleAlwaysOnBottom => tr("Keep the window below others").to_string(),
            Action::Move => tr("Move the window").to_string(),
            Action::Resize(_) => tr("Resize the window").to_string(),
            Action::MoveResizeTo { .. } | Action::MoveToEdge(_) => tr("Move the window").to_string(),
            Action::SnapToEdge(ResizeEdge::Left) => tr("Tile the window to the left half").to_string(),
            Action::SnapToEdge(ResizeEdge::Right) => tr("Tile the window to the right half").to_string(),
            Action::SnapToEdge(ResizeEdge::Top) => tr("Tile the window upward, or maximize it").to_string(),
            Action::SnapToEdge(_) => tr("Tile the window downward, or restore it").to_string(),
            Action::GoToDesktop(to) => {
                desktop_description("Next desktop", "Previous desktop", "Desktop", *to)
            }
            Action::SendToDesktop { to, .. } => desktop_description(
                "Move the window to the next desktop",
                "Move the window to the previous desktop",
                "Move the window to desktop",
                *to,
            ),
            Action::ToggleOmnipresent => tr("Window on all desktops").to_string(),
            Action::NextWindow => tr("Next window").to_string(),
            Action::PreviousWindow => tr("Previous window").to_string(),
            Action::ShowDesktop | Action::ToggleShowDesktop => tr("Show the desktop").to_string(),
            Action::Reconfigure => tr("Reconfigure the window manager").to_string(),
            Action::Unknown(name) => name.clone(),
        }
    }
}

/// The description of a desktop action: the next or previous key, or the numbered key with the
/// desktop's number (`Desktop 3`), the last desktop named as the next.
fn desktop_description(
    next: &'static str,
    previous: &'static str,
    numbered: &'static str,
    to: DesktopTarget,
) -> String {
    use crate::i18n::tr;
    match to {
        DesktopTarget::Next | DesktopTarget::Last => tr(next).to_string(),
        DesktopTarget::Previous => tr(previous).to_string(),
        DesktopTarget::Index(i) => format!("{} {}", tr(numbered), i + 1),
    }
}

/// A keysym with an ASCII letter lowered, so a bind on a letter matches the key with Shift held.
pub fn lower_keysym(keysym: u32) -> u32 {
    if (0x41..=0x5a).contains(&keysym) {
        keysym + 0x20
    } else {
        keysym
    }
}

fn parse_keybind(node: &Node) -> Option<KeyBind> {
    let key = node.attr("key")?;
    if key.split_whitespace().count() != 1 {
        log::debug!(target: "wm", "keybind {key:?}: chords are not implemented");
        return None;
    }
    let (modifiers, name) = parse_chord(key)?;
    let keysym = match keysym_from_name(name) {
        Some(sym) => sym,
        None => {
            log::debug!(target: "wm", "keybind {key:?}: unknown key {name:?}");
            return None;
        }
    };
    let on_release = node.attr("onRelease").map(truthy).unwrap_or(false);
    Some(KeyBind::new(
        modifiers,
        keysym,
        on_release,
        node.children_named("action").filter_map(parse_action).collect(),
    ))
}

/// The button number a bind's spelling names, openbox's `Left`, `Middle`, `Right`, `Up`, `Down`,
/// or `Button` followed by its number.
fn button_number(name: &str) -> Option<u32> {
    Some(match name.to_ascii_lowercase().as_str() {
        "left" => 1,
        "middle" => 2,
        "right" => 3,
        "up" => 4,
        "down" => 5,
        other => other.strip_prefix("button")?.parse().ok()?,
    })
}

fn parse_mousebind(context: Context, node: &Node) -> Option<MouseBind> {
    let spec = node.attr("button")?;
    let (modifiers, name) = parse_chord(spec)?;
    let button = button_number(name)?;
    let event = match node.attr("action")?.to_ascii_lowercase().as_str() {
        "press" => MouseEvent::Press,
        "release" => MouseEvent::Release,
        "click" => MouseEvent::Click,
        "doubleclick" => MouseEvent::DoubleClick,
        "drag" => MouseEvent::Drag,
        _ => return None,
    };
    Some(MouseBind {
        context,
        modifiers,
        button,
        event,
        actions: node.children_named("action").filter_map(parse_action).collect(),
    })
}

/// The direction an action's `<direction>` names.
fn direction(node: &Node) -> Direction {
    match node
        .child_text("direction")
        .map(|d| d.to_ascii_lowercase())
        .as_deref()
    {
        Some("horizontal") | Some("horz") => Direction::Horizontal,
        Some("vertical") | Some("vert") => Direction::Vertical,
        _ => Direction::Both,
    }
}

/// The edge an action's `<edge>` or `<direction>` names.
fn edge(node: &Node) -> Option<ResizeEdge> {
    let name = node.child_text("edge").or_else(|| node.child_text("direction"))?;
    Some(match name.to_ascii_lowercase().as_str() {
        "top" | "north" | "up" => ResizeEdge::Top,
        "bottom" | "south" | "down" => ResizeEdge::Bottom,
        "left" | "west" => ResizeEdge::Left,
        "right" | "east" => ResizeEdge::Right,
        "topleft" | "northwest" => ResizeEdge::TopLeft,
        "topright" | "northeast" => ResizeEdge::TopRight,
        "bottomleft" | "southwest" => ResizeEdge::BottomLeft,
        "bottomright" | "southeast" => ResizeEdge::BottomRight,
        _ => return None,
    })
}

/// The desktop a `<to>` child or attribute names.
fn desktop_target(node: &Node) -> Option<DesktopTarget> {
    node.child_text("to")
        .or_else(|| node.attr("to"))
        .and_then(DesktopTarget::parse)
}

/// The action an `<action name="...">` element describes, openbox's children or labwc's
/// attributes carrying its arguments.
pub fn parse_action(node: &Node) -> Option<Action> {
    let name = node.attr("name")?;
    let command = || {
        node.child_text("command")
            .or_else(|| node.child_text("execute"))
            .or_else(|| node.attr("command"))
            .map(str::to_string)
    };
    let menu = || {
        node.child_text("menu")
            .or_else(|| node.attr("menu"))
            .map(str::to_string)
    };
    Some(match name.to_ascii_lowercase().as_str() {
        "execute" => Action::Execute(command()?),
        "showmenu" => Action::ShowMenu(menu()?),
        "close" => Action::Close,
        "kill" => Action::Kill,
        "iconify" => Action::Iconify,
        "focus" => Action::Focus,
        "unfocus" => Action::Unfocus,
        "raise" => Action::Raise,
        "lower" => Action::Lower,
        "raiselower" => Action::RaiseLower,
        "focustobottom" => Action::FocusToBottom,
        "maximize" | "maximizefull" => Action::Maximize(direction(node)),
        "maximizehorz" => Action::Maximize(Direction::Horizontal),
        "maximizevert" => Action::Maximize(Direction::Vertical),
        "unmaximize" | "unmaximizefull" => Action::Unmaximize(direction(node)),
        "unmaximizehorz" => Action::Unmaximize(Direction::Horizontal),
        "unmaximizevert" => Action::Unmaximize(Direction::Vertical),
        "togglemaximize" | "togglemaximizefull" => Action::ToggleMaximize(direction(node)),
        "togglemaximizehorz" => Action::ToggleMaximize(Direction::Horizontal),
        "togglemaximizevert" => Action::ToggleMaximize(Direction::Vertical),
        "fullscreen" => Action::Fullscreen,
        "unfullscreen" => Action::Unfullscreen,
        "togglefullscreen" => Action::ToggleFullscreen,
        "shade" => Action::Shade,
        "unshade" => Action::Unshade,
        "toggleshade" => Action::ToggleShade,
        "decorate" => Action::Decorate,
        "undecorate" => Action::Undecorate,
        "toggledecorations" => Action::ToggleDecorations,
        "togglealwaysontop" => Action::ToggleAlwaysOnTop,
        "togglealwaysonbottom" => Action::ToggleAlwaysOnBottom,
        "move" => Action::Move,
        "resize" => Action::Resize(edge(node)),
        "moveresizeto" => Action::MoveResizeTo {
            x: node.child_text("x").and_then(Coordinate::parse),
            y: node.child_text("y").and_then(Coordinate::parse),
            width: node.child_text("width").and_then(|t| t.parse().ok()),
            height: node.child_text("height").and_then(|t| t.parse().ok()),
        },
        "movetoedge" => Action::MoveToEdge(edge(node)?),
        "snaptoedge" => Action::SnapToEdge(edge(node)?),
        "gotodesktop" => Action::GoToDesktop(desktop_target(node)?),
        "desktopnext" | "desktopright" => Action::GoToDesktop(DesktopTarget::Next),
        "desktopprevious" | "desktopleft" => Action::GoToDesktop(DesktopTarget::Previous),
        "desktoplast" => Action::GoToDesktop(DesktopTarget::Last),
        "sendtodesktop" | "senddesktop" => Action::SendToDesktop {
            to: desktop_target(node)?,
            follow: node.child_text("follow").map(truthy).unwrap_or(true),
        },
        "toggleomnipresent" => Action::ToggleOmnipresent,
        "nextwindow" => Action::NextWindow,
        "previouswindow" => Action::PreviousWindow,
        "showdesktop" => Action::ShowDesktop,
        "toggleshowdesktop" => Action::ToggleShowDesktop,
        "reconfigure" | "restart" => Action::Reconfigure,
        other => Action::Unknown(other.to_string()),
    })
}

fn parse_rule(node: &Node) -> Rule {
    let attr = |name: &str| node.attr(name).map(str::to_string);
    let mut rule = Rule {
        name: attr("name"),
        class: attr("class"),
        title: attr("title"),
        window_type: attr("type"),
        ..Rule::default()
    };
    rule.decor = node.yes("decor");
    if let Some(position) = node.child("position") {
        rule.position_forced = position.attr("force").map(truthy).unwrap_or(false);
        let x = position.child_text("x").and_then(Coordinate::parse);
        let y = position.child_text("y").and_then(Coordinate::parse);
        if let (Some(x), Some(y)) = (x, y) {
            rule.position = Some((x, y));
        }
        rule.monitor = position.child_text("monitor").and_then(|m| m.parse().ok());
    }
    if let Some(size) = node.child("size") {
        let w = size.child_text("width").and_then(|t| t.parse().ok());
        let h = size.child_text("height").and_then(|t| t.parse().ok());
        if let (Some(w), Some(h)) = (w, h) {
            rule.size = Some((w, h));
        }
    }
    rule.focus = node.yes("focus");
    rule.layer = node
        .child_text("layer")
        .and_then(|l| match l.to_ascii_lowercase().as_str() {
            "above" => Some(RuleLayer::Above),
            "below" => Some(RuleLayer::Below),
            "normal" => Some(RuleLayer::Normal),
            _ => None,
        });
    rule.iconic = node.yes("iconic");
    rule.skip_taskbar = node.yes("skip_taskbar");
    rule.fullscreen = node.yes("fullscreen");
    rule.maximized = node
        .child_text("maximized")
        .map(|m| truthy(m) || m.eq_ignore_ascii_case("horizontal") || m.eq_ignore_ascii_case("vertical"));
    rule.shade = node.yes("shade");
    rule
}

/// One entry of a menu.
#[derive(Clone, Debug, PartialEq)]
pub enum MenuItem {
    Item {
        label: String,
        icon: Option<String>,
        actions: Vec<Action>,
    },
    Separator {
        label: Option<String>,
    },
    /// A submenu, by the id of the menu it opens.
    Submenu {
        id: String,
        label: Option<String>,
        icon: Option<String>,
    },
}

/// A menu `menu.xml` defines, or a pipe menu names.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Menu {
    pub id: String,
    pub label: String,
    pub icon: Option<String>,
    /// The command whose output is the menu's content, read when the menu opens.
    pub execute: Option<String>,
    pub items: Vec<MenuItem>,
}

/// Every menu the files define, by id. The ones the window manager fills itself are resolved
/// by the engine whatever a file says of them: `client-menu`, `client-list-combined-menu`, and
/// `selkies-applications`, the installed applications by category with their icons, which the
/// images' files name as a pipe menu for openbox and labwc.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Menus {
    pub menus: HashMap<String, Menu>,
    pub sources: Vec<PathBuf>,
}

impl Menus {
    /// The menus of every file the configuration names, found along its search directories.
    pub fn load(config: &Config) -> Menus {
        let mut menus = Menus::default();
        for file in &config.menu_files {
            let Some(path) = find_file(&config.search_dirs, file) else {
                log::warn!(target: "wm", "menu file {file} not found");
                continue;
            };
            match std::fs::read_to_string(&path) {
                Ok(text) => match menus.apply(&text) {
                    Ok(()) => menus.sources.push(path),
                    Err(e) => log::warn!(target: "wm", "{}: not read: {e}", path.display()),
                },
                Err(e) => log::warn!(target: "wm", "{}: not read: {e}", path.display()),
            }
        }
        menus
    }

    /// Fold a `menu.xml` document in: every `<menu>` with content defines a menu, wherever it
    /// sits.
    pub fn apply(&mut self, text: &str) -> Result<(), String> {
        let root = xml::parse(text)?;
        for node in root.children_named("menu") {
            self.define(node);
        }
        Ok(())
    }

    fn define(&mut self, node: &Node) -> Option<String> {
        let id = node.attr("id")?.to_string();
        let defines =
            node.attr("execute").is_some() || node.attr("label").is_some() || !node.children.is_empty();
        if !defines {
            return Some(id);
        }
        let items = self.items_of(node);
        self.menus.insert(
            id.clone(),
            Menu {
                id: id.clone(),
                label: node.attr("label").unwrap_or("").to_string(),
                icon: node.attr("icon").map(str::to_string),
                execute: node.attr("execute").map(str::to_string),
                items,
            },
        );
        Some(id)
    }

    /// The entries of a menu element, submenus defined on the way.
    fn items_of(&mut self, node: &Node) -> Vec<MenuItem> {
        let mut items = Vec::new();
        for child in &node.children {
            match child.name.to_ascii_lowercase().as_str() {
                "item" => items.push(MenuItem::Item {
                    label: child.attr("label").unwrap_or("").to_string(),
                    icon: child.attr("icon").map(str::to_string),
                    actions: child.children_named("action").filter_map(parse_action).collect(),
                }),
                "separator" => items.push(MenuItem::Separator {
                    label: child.attr("label").map(str::to_string),
                }),
                "menu" => {
                    if let Some(id) = self.define(child) {
                        items.push(MenuItem::Submenu {
                            id,
                            label: child.attr("label").map(str::to_string),
                            icon: child.attr("icon").map(str::to_string),
                        });
                    }
                }
                _ => {}
            }
        }
        items
    }

    /// The entries a pipe menu printed: an `<openbox_pipe_menu>` document.
    pub fn parse_pipe(&mut self, text: &str) -> Result<Vec<MenuItem>, String> {
        let root = xml::parse(text)?;
        Ok(self.items_of(&root))
    }
}

/// The bindings openbox's stock `rc.xml` carries, the ones an image's additions sit beside, with
/// Super and an arrow quick-tiling the focused window, Control added switching the virtual
/// desktop, and Shift added as well taking the window along.
pub fn default_keybinds() -> Vec<KeyBind> {
    let bind = |spec: &str, on_release: bool, actions: Vec<Action>| {
        let (modifiers, keysym) = parse_spec(spec).expect("a default chord parses");
        KeyBind::new(modifiers, keysym, on_release, actions)
    };
    vec![
        bind("A-F4", false, vec![Action::Close]),
        bind(
            "A-Escape",
            false,
            vec![Action::Lower, Action::FocusToBottom, Action::Unfocus],
        ),
        bind(
            "A-space",
            false,
            vec![Action::ShowMenu("client-menu".to_string())],
        ),
        bind("A-Tab", false, vec![Action::NextWindow]),
        bind("A-S-Tab", false, vec![Action::PreviousWindow]),
        bind("W-a", false, vec![Action::ToggleMaximize(Direction::Both)]),
        bind("W-Up", false, vec![Action::SnapToEdge(ResizeEdge::Top)]),
        bind("W-Down", false, vec![Action::SnapToEdge(ResizeEdge::Bottom)]),
        bind("W-Left", false, vec![Action::SnapToEdge(ResizeEdge::Left)]),
        bind("W-Right", false, vec![Action::SnapToEdge(ResizeEdge::Right)]),
        bind(
            "C-W-Left",
            false,
            vec![Action::GoToDesktop(DesktopTarget::Previous)],
        ),
        bind("C-W-Right", false, vec![Action::GoToDesktop(DesktopTarget::Next)]),
        bind(
            "C-W-S-Left",
            false,
            vec![Action::SendToDesktop {
                to: DesktopTarget::Previous,
                follow: true,
            }],
        ),
        bind(
            "C-W-S-Right",
            false,
            vec![Action::SendToDesktop {
                to: DesktopTarget::Next,
                follow: true,
            }],
        ),
    ]
}

/// The pointer bindings openbox's stock `rc.xml` carries.
pub fn default_mousebinds() -> Vec<MouseBind> {
    let mut binds = Vec::new();
    let mut add = |contexts: &[Context], spec: &str, event: MouseEvent, actions: &[Action]| {
        let (modifiers, name) = parse_chord(spec).expect("a default chord parses");
        let button = button_number(name).expect("a default button is known");
        for &context in contexts {
            binds.push(MouseBind {
                context,
                modifiers,
                button,
                event,
                actions: actions.to_vec(),
            });
        }
    };
    use Action::*;
    use Context as C;
    use MouseEvent::*;
    let client_menu = || ShowMenu("client-menu".to_string());
    add(&[C::Frame], "A-Left", Press, &[Focus, Raise]);
    add(&[C::Frame], "A-Left", Click, &[Unshade]);
    add(&[C::Frame], "A-Left", Drag, &[Move]);
    add(&[C::Frame], "A-Right", Press, &[Focus, Raise, Unshade]);
    add(&[C::Frame], "A-Right", Drag, &[Resize(None)]);
    add(&[C::Frame], "A-Middle", Press, &[Lower, FocusToBottom, Unfocus]);
    add(&[C::Titlebar], "Left", Drag, &[Move]);
    add(
        &[C::Titlebar],
        "Left",
        DoubleClick,
        &[ToggleMaximize(Direction::Both)],
    );
    let edges = [
        C::Titlebar,
        C::Top,
        C::Right,
        C::Bottom,
        C::Left,
        C::TLCorner,
        C::TRCorner,
        C::BRCorner,
        C::BLCorner,
    ];
    add(&edges, "Left", Press, &[Focus, Raise, Unshade]);
    add(&edges, "Middle", Press, &[Lower, FocusToBottom, Unfocus]);
    add(&edges, "Right", Press, &[Focus, Raise, client_menu()]);
    add(&[C::Top], "Left", Drag, &[Resize(Some(ResizeEdge::Top))]);
    add(&[C::Left], "Left", Drag, &[Resize(Some(ResizeEdge::Left))]);
    add(&[C::Right], "Left", Drag, &[Resize(Some(ResizeEdge::Right))]);
    add(&[C::Bottom], "Left", Drag, &[Resize(Some(ResizeEdge::Bottom))]);
    add(&[C::TLCorner], "Left", Drag, &[Resize(Some(ResizeEdge::TopLeft))]);
    add(
        &[C::TRCorner],
        "Left",
        Drag,
        &[Resize(Some(ResizeEdge::TopRight))],
    );
    add(
        &[C::BLCorner],
        "Left",
        Drag,
        &[Resize(Some(ResizeEdge::BottomLeft))],
    );
    add(
        &[C::BRCorner],
        "Left",
        Drag,
        &[Resize(Some(ResizeEdge::BottomRight))],
    );
    add(&[C::Client], "Left", Press, &[Focus, Raise]);
    add(&[C::Client], "Middle", Press, &[Focus, Raise]);
    add(&[C::Client], "Right", Press, &[Focus, Raise]);
    add(&[C::Icon], "Left", Press, &[Focus, Raise, Unshade, client_menu()]);
    add(&[C::Icon], "Right", Press, &[Focus, Raise, client_menu()]);
    add(&[C::Shade], "Left", Press, &[Focus, Raise]);
    add(&[C::Shade], "Left", Click, &[ToggleShade]);
    add(&[C::Iconify], "Left", Press, &[Focus, Raise]);
    add(&[C::Iconify], "Left", Click, &[Iconify]);
    add(&[C::Maximize], "Left", Press, &[Focus, Raise, Unshade]);
    add(&[C::Maximize], "Middle", Press, &[Focus, Raise, Unshade]);
    add(&[C::Maximize], "Right", Press, &[Focus, Raise, Unshade]);
    add(&[C::Maximize], "Left", Click, &[ToggleMaximize(Direction::Both)]);
    add(
        &[C::Maximize],
        "Middle",
        Click,
        &[ToggleMaximize(Direction::Vertical)],
    );
    add(
        &[C::Maximize],
        "Right",
        Click,
        &[ToggleMaximize(Direction::Horizontal)],
    );
    add(&[C::Close], "Left", Press, &[Focus, Raise, Unshade]);
    add(&[C::Close], "Left", Click, &[Close]);
    add(&[C::Desktop], "Left", Press, &[Focus, Raise]);
    add(&[C::Desktop], "Right", Press, &[Focus, Raise]);
    add(
        &[C::Root],
        "Middle",
        Press,
        &[ShowMenu("client-list-combined-menu".to_string())],
    );
    add(&[C::Root], "Right", Press, &[ShowMenu("root-menu".to_string())]);
    binds
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A chord is shown with its modifiers in one order and the key as a user names it, and an
    /// action describes itself for the shortcuts list.
    #[test]
    fn chords_and_actions_describe_themselves() {
        let (modifiers, key) = parse_chord("C-A-t").unwrap();
        assert_eq!(
            chord_label(modifiers, keysym_from_name(key).unwrap()),
            "Ctrl+Alt+T"
        );
        let (modifiers, key) = parse_chord("W-S-Left").unwrap();
        assert_eq!(
            chord_label(modifiers, keysym_from_name(key).unwrap()),
            "Shift+Super+Left"
        );
        assert_eq!(
            chord_label(Modifiers::default(), keysym_from_name("Super_L").unwrap()),
            "Super"
        );
        assert_eq!(
            chord_label(Modifiers::default(), keysym_from_name("space").unwrap()),
            "Space"
        );
        assert_eq!(
            Action::Execute("selkies-desktop --launcher".into()).description(),
            "Quick launcher"
        );
        assert_eq!(
            Action::SnapToEdge(ResizeEdge::Left).description(),
            "Tile the window to the left half"
        );
        assert_eq!(
            Action::GoToDesktop(DesktopTarget::Index(2)).description(),
            "Desktop 3"
        );
        assert_eq!(canonical_spec("Super-S-Left").as_deref(), Some("S-W-Left"));
        assert_eq!(canonical_spec("C-A-T").as_deref(), Some("C-A-t"));
    }

    /// An override moves a bind to another chord by the chord the configuration gave it,
    /// disables one with an empty replacement, adds a command bind, and skips what it cannot
    /// read; the bind keeps its origin so a later override still finds it.
    #[test]
    fn overrides_rebind_disable_and_add_binds() {
        let mut config = Config::default();
        let applied = config.apply_overrides(
            "# the user's\nrebind\tW-Left\tC-W-S-h\nrebind\tA-F4\t\nrebind\tW-nope\tW-x\n\
             execute\tW-Return\tx-terminal-emulator\nexecute\tW-bogus_key\tfoo\nwhat\tever\n",
        );
        assert_eq!(applied, 3);
        let left = config
            .keybinds
            .iter()
            .find(|b| b.origin == "W-Left")
            .expect("the tile bind keeps its origin");
        assert_eq!(left.spec(), "C-S-W-h");
        assert!(!left.disabled);
        let close = config.keybinds.iter().find(|b| b.origin == "A-F4").unwrap();
        assert!(close.disabled);
        assert_eq!(close.spec(), "");
        let custom = config.keybinds.iter().find(|b| b.custom()).unwrap();
        assert_eq!(custom.spec(), "W-Return");
        assert_eq!(
            custom.actions,
            vec![Action::Execute("x-terminal-emulator".to_string())]
        );
        assert_eq!(Action::Execute("xterm".into()).description(), "Run command xterm");
    }

    /// The image's labwc additions sit beside the defaults, a bind on a key the defaults hold
    /// replacing it, and the labwc spellings are read.
    #[test]
    fn the_images_additions_sit_beside_the_defaults() {
        let mut config = Config::default();
        let defaults = config.keybinds.len();
        config
            .apply_rc(
                r#"<labwc_config>
  <core><decoration>server</decoration></core>
  <placement><policy>cascade</policy></placement>
  <keyboard>
    <default />
    <keybind key="Super_L" onRelease="yes"><action name="Execute" command="selkies-desktop --toggle-menu" /></keybind>
    <keybind key="C-A-t"><action name="Execute" command="x-terminal-emulator" /></keybind>
    <keybind key="A-F4"><action name="Kill" /></keybind>
  </keyboard>
  <mouse>
    <default />
    <context name="Root">
      <mousebind button="Left" action="Press" />
      <mousebind button="Middle" action="Press"><action name="ShowMenu" menu="client-list-combined-menu" /></mousebind>
    </context>
  </mouse>
</labwc_config>"#,
            )
            .unwrap();
        assert_eq!(config.placement, PlacementPolicy::Cascade);
        assert!(config.decorate_by_default);
        assert_eq!(config.keybinds.len(), defaults + 2);
        let super_l = config
            .keybinds
            .iter()
            .find(|b| b.keysym == keysym_from_name("Super_L").unwrap())
            .unwrap();
        assert!(super_l.on_release);
        assert_eq!(
            super_l.actions,
            vec![Action::Execute("selkies-desktop --toggle-menu".to_string())]
        );
        let f4 = config
            .keybinds
            .iter()
            .find(|b| b.keysym == keysym_from_name("F4").unwrap() && b.modifiers.alt)
            .unwrap();
        assert_eq!(f4.actions, vec![Action::Kill]);
        let root: Vec<&MouseBind> = config
            .mousebinds
            .iter()
            .filter(|b| b.context == Context::Root)
            .collect();
        assert_eq!(root.len(), 2);
        assert!(root.iter().all(|b| b.button != 1));
    }

    /// A full openbox mouse section replaces the defaults of the contexts it names and keeps the
    /// others; a letter bind matches with Shift held.
    #[test]
    fn an_explicit_mouse_section_replaces_its_contexts() {
        let mut config = Config::default();
        config
            .apply_rc(
                r#"<openbox_config xmlns="http://openbox.org/3.4/rc">
  <mouse>
    <context name="Titlebar"><mousebind button="Left" action="Drag"><action name="Move"/></mousebind></context>
  </mouse>
  <keyboard><keybind key="W-T"><action name="Execute"><command>xterm</command></action></keybind></keyboard>
</openbox_config>"#,
            )
            .unwrap();
        let titlebar: Vec<&MouseBind> = config
            .mousebinds
            .iter()
            .filter(|b| b.context == Context::Titlebar)
            .collect();
        assert_eq!(titlebar.len(), 1);
        assert!(config.mousebinds.iter().any(|b| b.context == Context::Close));
        assert_eq!(config.keybinds.len(), 1);
        assert_eq!(config.keybinds[0].keysym, keysym_from_name("t").unwrap());
        assert!(config.keybinds[0].modifiers.logo);
    }

    /// The image's menu defines its menus, a pipe menu by its command, and the pipe menu's
    /// output parses into entries.
    #[test]
    fn the_images_menu_and_a_pipe_menu_parse() {
        let mut menus = Menus::default();
        menus
            .apply(
                r#"<openbox_menu>
  <menu id="root-menu">
    <item label="Terminal" icon="utilities-terminal"><action name="Execute"><command>x-terminal-emulator</command></action></item>
    <separator />
    <menu id="selkies-applications" label="Applications" execute="pipe-menu --icons" />
    <menu id="client-list-combined-menu" />
    <item label="Reconfigure"><action name="Reconfigure" /></item>
  </menu>
</openbox_menu>"#,
            )
            .unwrap();
        let root = &menus.menus["root-menu"];
        assert_eq!(root.items.len(), 5);
        assert!(matches!(&root.items[2], MenuItem::Submenu { id, .. } if id == "selkies-applications"));
        assert!(matches!(&root.items[3], MenuItem::Submenu { id, .. } if id == "client-list-combined-menu"));
        assert_eq!(
            menus.menus["selkies-applications"].execute.as_deref(),
            Some("pipe-menu --icons")
        );
        let items = menus
            .parse_pipe(
                r#"<openbox_pipe_menu><menu id="pipe-Office" label="Office"><item label="Writer"><action name="Execute"><command>lowriter</command></action></item></menu></openbox_pipe_menu>"#,
            )
            .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(menus.menus["pipe-Office"].items.len(), 1);
    }

    /// Rules match by glob on the class and the title, and carry what they set.
    #[test]
    fn rules_match_by_glob() {
        let mut config = Config::default();
        config
            .apply_rc(
                r#"<openbox_config><applications>
  <application class="Fire*" type="normal"><decor>no</decor><maximized>yes</maximized><position force="yes"><x>center</x><y>-10</y></position></application>
</applications></openbox_config>"#,
            )
            .unwrap();
        let rule = &config.rules[0];
        assert!(rule.matches("Navigator", "Firefox", "Mozilla Firefox", "normal"));
        assert!(!rule.matches("Navigator", "Firefox", "Mozilla Firefox", "dialog"));
        assert!(!rule.matches("x", "foot", "", "normal"));
        assert_eq!(rule.decor, Some(false));
        assert_eq!(rule.maximized, Some(true));
        assert_eq!(rule.position, Some((Coordinate::Center, Coordinate::FromEnd(10))));
        assert!(rule.position_forced);
        assert_eq!(Coordinate::FromEnd(10).resolve(100, 1000, 200), 890);
        assert!(glob_match("a?c*", "AbCdef"));
        assert!(!glob_match("a?c", "ac"));
    }
}
