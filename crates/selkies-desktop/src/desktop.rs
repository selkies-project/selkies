//! The desktop's state: the window list, the loaded applications, what is
//! open and hovered, and the frames it renders for the backend to present.
//!
//! The backends feed [`Event`]s in and take [`Request`]s out; everything
//! between is display server agnostic.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tiny_skia::Pixmap;

use crate::config::Config;
use crate::entries::{self, Apps};
use crate::event::{Button, Event, Modifiers, Request, SurfaceId, ToplevelState, WindowAction};
use crate::files;
use crate::geom::Rect;
use crate::i18n::{tr, trf};
use crate::icons::Icons;
use crate::render::{Canvas, Fonts};
use crate::ui::settings::Page;
use crate::ui::{self, background, launcher, menu, panel, popups, settings};
use crate::xkb::Key;

/// Height of the strip that brings an auto-hidden bar back.
const REVEAL_STRIP: i32 = 3;
/// Milliseconds the pointer is away from the bar before it hides.
const HIDE_DELAY_MS: u64 = 500;
/// How long the height and opacity sliders rest before their value is
/// applied to the bar, so a drag does not resize the bar, and every window
/// with it, at every pointer step; the width follows the pointer live.
const PREVIEW_DELAY_MS: u64 = 150;
/// Logical pixels cleared and damaged around what the bar and its popups
/// draw, as the narrow bar's outline is stroked on its edge and so reaches
/// a pixel outside it.
const CLEAR_MARGIN: i32 = 2;

/// A window the window manager reports.
#[derive(Debug)]
pub struct Toplevel {
    /// Backend assigned identifier.
    pub id: u64,
    /// Window title.
    pub title: String,
    /// Application ID or window class.
    pub app_id: String,
    /// Icon the window published itself, at its native size.
    pub own_icon: Option<Pixmap>,
    /// Window state flags.
    pub state: ToplevelState,
    /// The themed icon resolved for the current scale.
    icon: Option<(i32, Option<Arc<Pixmap>>)>,
}

impl Toplevel {
    /// The text the taskbar shows for the window.
    pub fn label(&self) -> &str {
        if !self.title.is_empty() {
            &self.title
        } else if !self.app_id.is_empty() {
            &self.app_id
        } else {
            tr("Window")
        }
    }
}

/// Size and scale of one surface.
#[derive(Clone, Copy, Debug, Default)]
pub struct Geometry {
    /// Logical width.
    pub width: i32,
    /// Logical height.
    pub height: i32,
    /// Output scale.
    pub scale: i32,
    /// Whether the surface has been given a size.
    pub configured: bool,
}

/// What a rendered wallpaper layer was made for: size, scale and monitors.
pub type WallpaperKey = (i32, i32, i32, Vec<Rect>);

/// One wallpaper surface: an output on Wayland, the whole screen on X11,
/// with the monitors it spans.
pub struct BackgroundSurface {
    /// The backend's number for the surface.
    pub id: u32,
    /// Size and scale.
    pub geometry: Geometry,
    /// The monitors the surface spans, in its logical coordinates; the first
    /// one carries the desktop icons when the surface is primary.
    pub monitors: Vec<Rect>,
    /// Whether this surface shows the desktop icons.
    pub primary: bool,
    pix: Option<Pixmap>,
    launchers_pix: Option<Pixmap>,
    dirty: bool,
    /// The wallpaper layer rendered for the geometry and monitors, drawn
    /// under the icons.
    pub wallpaper_cache: Option<(WallpaperKey, Pixmap)>,
}

impl BackgroundSurface {
    /// The monitor rectangle the desktop icons are laid out in.
    pub fn icon_area(&self) -> Rect {
        self.monitors
            .first()
            .copied()
            .unwrap_or(Rect::new(0, 0, self.geometry.width, self.geometry.height))
    }
}

/// A rendered surface handed to the backend.
pub struct Frame<'a> {
    /// The pixels, premultiplied RGBA, logical size times scale.
    pub pixmap: &'a Pixmap,
    /// Output scale the pixmap was rendered at.
    pub scale: i32,
    /// Logical rectangles that changed since the previous frame.
    pub damage: Vec<Rect>,
    /// Logical rectangles that take input; everything else falls through.
    pub input: Vec<Rect>,
    /// Logical rectangles with content; the rest is transparent.
    pub visible: Vec<Rect>,
    /// Whether the surface wants every click on the screen while a popup is open.
    pub grab: bool,
}

/// What a right click on the bar landed on, which decides the menu it gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelTarget {
    /// The bar itself, between its items.
    Bar,
    /// The start button.
    Start,
    /// A quick launch button, by position.
    Launcher(usize),
    /// The pager.
    Pager,
    /// The clock.
    Clock,
    /// The volume control.
    Volume,
    /// The system tray strip.
    Tray,
    /// The show desktop button.
    ShowDesktop,
}

/// Which popup is open above the bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Popup {
    /// Nothing is open.
    None,
    /// The start menu.
    Menu,
    /// The quick launcher.
    Launcher,
    /// The volume slider.
    Volume,
    /// The month calendar.
    Calendar,
    /// The context menu of a taskbar button, by toplevel index.
    TaskMenu(usize),
    /// The bar's right-click menu, by the bar X it opened at and the item
    /// it was opened on.
    PanelMenu(i32, PanelTarget),
}

/// What the pointer rests on, for hover highlights and tooltips.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hover {
    /// Nothing interactive.
    None,
    /// The start button.
    Start,
    /// A taskbar button, by toplevel index.
    Task(usize),
    /// The volume control.
    Volume,
    /// The clock.
    Clock,
    /// The show desktop button.
    ShowDesktop,
    /// A menu category row.
    MenuCategory(usize),
    /// A menu application row, or a cell of the application grid.
    MenuApp(usize),
    /// A favorite's tile in the menu's tiles layout.
    MenuTile(usize),
    /// An item of the task context menu.
    TaskMenuItem(usize),
    /// An item of the bar's right-click menu.
    PanelMenuItem(usize),
    /// A control of the settings window.
    Control(usize),
    /// A desktop launcher cell.
    DesktopIcon(usize),
    /// A quick launch button, by position.
    Launcher(usize),
    /// A pill of the pager, by desktop index.
    Pager(usize),
    /// An item of the start menu's right-click menu.
    MenuContextItem(usize),
    /// A result row of the quick launcher.
    LauncherRow(usize),
    /// Something in a file manager window.
    Files(u32, ui::files::Hit),
}

/// Current volume as the audio module reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Volume {
    /// Percent, may exceed 100.
    pub percent: u8,
    /// Whether the sink is muted.
    pub muted: bool,
}

/// The whole desktop.
pub struct Desktop {
    /// Settings.
    pub config: Config,
    /// Font system.
    pub fonts: Fonts,
    /// Icon lookup.
    pub icons: Icons,
    /// Loaded applications.
    pub apps: Apps,
    /// The Selkies logo, if the image ships one.
    pub logo: Option<Pixmap>,
    /// Wallpaper images at native size, by path; `None` for files that did not decode.
    pub wallpapers: HashMap<std::path::PathBuf, Option<Arc<Pixmap>>>,
    /// Geometry of the panel surface.
    pub panel: Geometry,
    /// The wallpaper surfaces, one per output.
    pub backgrounds: Vec<BackgroundSurface>,
    panel_pix: Option<Pixmap>,
    panel_visible: Vec<Rect>,
    panel_dirty: bool,
    /// The task button rectangles last told to the backend.
    published_tasks: Vec<(u64, Rect)>,
    /// Windows in order of appearance.
    pub toplevels: Vec<Toplevel>,
    /// The open popup.
    pub popup: Popup,
    /// What the pointer is over.
    pub hover: Hover,
    hover_since: Instant,
    /// Tooltip text shown at the moment, if any.
    pub tooltip: Option<String>,
    /// Last pointer position on the panel.
    pub pointer: (f64, f64),
    /// Start menu state.
    pub menu: menu::MenuState,
    /// Quick launcher state.
    pub launcher: launcher::LauncherState,
    /// The settings window and its state.
    pub settings: settings::SettingsState,
    /// Selected desktop icon.
    pub desktop_selection: Option<usize>,
    last_click: Option<(u32, usize)>,
    /// Windows minimized by the show desktop button, to restore on the next press.
    pub shown_desktop: Vec<u64>,
    /// Volume, or `None` while audio is unavailable.
    pub volume: Option<Volume>,
    /// Whether the volume slider is being dragged.
    pub volume_drag: bool,
    /// The clock text.
    pub clock: String,
    /// The month the calendar shows, offset in months from the current one.
    pub calendar_offset: i32,
    /// Requests for the backend, taken by the main loop.
    pub requests: Vec<Request>,
    cursor_pointer: bool,
    /// Which desktop launcher icons are resolved, by index and scale.
    desktop_icons: HashMap<(usize, i32), Option<Arc<Pixmap>>>,
    /// The file manager.
    pub files: files::Manager,
    /// The start button icon resolved for the current setting and size.
    start_icon: Option<(Option<String>, u32, Option<Arc<Pixmap>>)>,
    /// Whether an auto-hiding bar is tucked away.
    panel_hidden: bool,
    /// When a hiding bar goes away, after the pointer left it.
    hide_at: Option<Instant>,
    /// A slider value not yet applied to the bar, with the key it is for and
    /// when the drag has rested long enough to apply it.
    preview: Option<(String, i32, Instant)>,
    /// Whether the display cannot show alpha, so the bar is blended over
    /// the wallpaper before it is handed over, as on X11 without a
    /// compositing manager.
    pub opaque_output: bool,
    /// Where the panel surface's top-left corner sits in the primary
    /// background's logical coordinates, for that blending.
    pub panel_offset: (i32, i32),
    /// The wallpaper image shrunk for the settings window's theme preview,
    /// with the path and scale it was made for.
    pub preview_wallpaper: Option<(std::path::PathBuf, i32, Arc<Pixmap>)>,
    /// Icons docked in the system tray.
    pub tray_icons: usize,
    /// The strip of the bar the tray icons occupy, in panel coordinates.
    pub tray_slot: Option<Rect>,
    /// Whether the tray strip moved since the tray was last placed.
    pub tray_moved: bool,
    /// The desktop Settings portal the shell serves, when it holds the name.
    pub portal: Option<crate::portal::Portal>,
    /// How many virtual desktops the window manager reports.
    pub desktops: u32,
    /// The index of the current virtual desktop.
    pub current_desktop: u32,
    /// The name of the display backend the shell runs on, for the About page.
    pub backend: String,
}

impl Desktop {
    /// Loads settings, fonts, icons and applications, and applies the
    /// settings to the session: the window manager's theme, and the file
    /// manager's desktop entry and folder default.
    pub fn new() -> Desktop {
        Desktop::load(true)
    }

    /// Loads settings, fonts, icons and applications.
    ///
    /// # Arguments
    /// * `session` - Whether to apply the settings to the session as well,
    ///   as `new` does; without it nothing is written to the user's files
    ///   and no window manager is signaled, which is how tests build one.
    pub fn load(session: bool) -> Desktop {
        let config = Config::load();
        ui::set_theme(config.theme);
        ui::set_style(config.window);
        let mut portal = None;
        if session {
            crate::i18n::select(config.language.as_deref());
            crate::wmtheme::apply(&config);
            let decision = crate::toolkits::apply(&config);
            portal = crate::portal::Portal::start(&decision);
        }
        let fonts = Fonts::new();
        let icons = Icons::new(config.icon_theme.as_deref());
        if session && config.files_manager {
            entries::ensure_files_entry();
        }
        if session {
            entries::ensure_settings_entry();
        }
        let apps = Apps::load(entries::find_terminal(&config).is_some());
        let logo = crate::render::load_raster(std::path::Path::new(crate::config::LOGO_PATH), 0);
        let mut desktop = Desktop {
            config,
            fonts,
            icons,
            apps,
            logo,
            wallpapers: HashMap::new(),
            panel: Geometry::default(),
            backgrounds: Vec::new(),
            panel_pix: None,
            panel_visible: Vec::new(),
            panel_dirty: false,
            published_tasks: Vec::new(),
            toplevels: Vec::new(),
            popup: Popup::None,
            hover: Hover::None,
            hover_since: Instant::now(),
            tooltip: None,
            pointer: (-1.0, -1.0),
            menu: menu::MenuState::default(),
            launcher: launcher::LauncherState::default(),
            settings: settings::SettingsState::default(),
            desktop_selection: None,
            last_click: None,
            shown_desktop: Vec::new(),
            volume: None,
            volume_drag: false,
            clock: String::new(),
            calendar_offset: 0,
            requests: Vec::new(),
            cursor_pointer: false,
            desktop_icons: HashMap::new(),
            files: files::Manager::new(),
            start_icon: None,
            panel_hidden: false,
            hide_at: None,
            preview: None,
            opaque_output: false,
            panel_offset: (0, 0),
            preview_wallpaper: None,
            tray_icons: 0,
            tray_slot: None,
            tray_moved: false,
            portal,
            desktops: 1,
            current_desktop: 0,
            backend: String::new(),
        };
        desktop.requests.push(desktop.panel_request());
        desktop
    }

    /// The space the bar reserves along its edge: its height and the margin
    /// that floats it.
    pub fn reserved_height(&self) -> i32 {
        self.config.panel_height + self.config.panel_margin
    }

    /// The request that places the bar as the settings have it.
    fn panel_request(&self) -> Request {
        Request::Panel {
            top: self.panel_top(),
            height: self.reserved_height(),
            autohide: self.config.panel_autohide,
            margin: self.config.panel_margin,
        }
    }

    /// Whether the bar is tucked away at the moment.
    pub fn panel_hidden(&self) -> bool {
        self.config.panel_autohide && self.panel_hidden && self.popup == Popup::None
    }

    /// The strip along the edge that brings a hidden bar back.
    pub fn reveal_strip(&self) -> Rect {
        let bar = self.bar_rect();
        if self.panel_top() {
            Rect::new(bar.x, 0, bar.w, REVEAL_STRIP)
        } else {
            Rect::new(bar.x, self.panel.height - REVEAL_STRIP, bar.w, REVEAL_STRIP)
        }
    }

    /// Shows a hidden bar and cancels a pending hide.
    fn reveal_panel(&mut self) {
        self.hide_at = None;
        if self.panel_hidden {
            self.panel_hidden = false;
            self.panel_dirty = true;
        }
    }

    /// Starts the timer that hides the bar once the pointer has left it.
    fn schedule_hide(&mut self) {
        if self.config.panel_autohide && self.popup == Popup::None && !self.panel_hidden {
            self.hide_at = Some(Instant::now() + Duration::from_millis(HIDE_DELAY_MS));
        }
    }

    /// Records how many icons the system tray holds.
    ///
    /// # Arguments
    /// * `count` - The number of docked icons.
    pub fn set_tray_icons(&mut self, count: usize) {
        if self.tray_icons != count {
            self.tray_icons = count;
            self.panel_dirty = true;
        }
    }

    /// Applies a bar or window setting on screen without writing it, while
    /// a slider is dragged.
    ///
    /// # Arguments
    /// * `key` - `panel_height`, `panel_width`, `panel_opacity`,
    ///   `panel_margin` or `window_corners`.
    /// * `value` - The value.
    pub fn preview_panel(&mut self, key: &str, value: i32) {
        let rest = if key == "panel_width" {
            Duration::ZERO
        } else {
            Duration::from_millis(PREVIEW_DELAY_MS)
        };
        self.preview = Some((key.to_string(), value, Instant::now() + rest));
        if rest.is_zero() {
            self.apply_preview();
        }
        self.panel_dirty = true;
    }

    /// Forgets a slider value not yet applied, when the drag ends and the
    /// setting is written instead.
    pub fn cancel_preview(&mut self) {
        self.preview = None;
    }

    /// Applies the slider value whose rest time has passed.
    fn apply_preview(&mut self) {
        let Some((key, value, _)) = self.preview.take() else {
            return;
        };
        let (was_top, was_height, was_autohide) = (
            self.panel_top(),
            self.reserved_height(),
            self.config.panel_autohide,
        );
        match key.as_str() {
            "panel_height" => self.config.panel_height = value,
            "panel_width" => self.config.panel_width = value,
            "panel_opacity" => self.config.panel_opacity = value,
            "panel_margin" => self.config.panel_margin = value,
            "window_corners" => self.config.window.corners = value,
            _ => return,
        }
        self.sync_panel(was_top, was_height, was_autohide);
        self.panel_dirty = true;
    }

    /// Icon size of the bar's items, grown with the bar.
    pub fn panel_icon_size(&self) -> i32 {
        (self.config.panel_height * ui::ICON_SIZE / ui::PANEL_HEIGHT).clamp(16, 36)
    }

    /// The start button's icon at the current scale: the configured icon
    /// name or image, else the Selkies logo.
    ///
    /// # Arguments
    /// * `size` - Logical size wanted.
    pub fn start_icon(&mut self, size: i32) -> Option<Arc<Pixmap>> {
        let px = (size * self.panel.scale.max(1)) as u32;
        let wanted = self.config.start_icon.clone();
        if let Some((name, cached_px, icon)) = &self.start_icon
            && *name == wanted
            && *cached_px == px
        {
            return icon.clone();
        }
        let icon = match &wanted {
            Some(name) => self.icons.get(name, px),
            None => None,
        }
        .or_else(|| {
            self.logo
                .as_ref()
                .and_then(|logo| crate::render::resize_pixmap(logo, px))
                .map(Arc::new)
        });
        self.start_icon = Some((wanted, px, icon.clone()));
        icon
    }

    /// Tells the backend where the bar goes when the edge, reserved height
    /// or hiding changed.
    ///
    /// # Arguments
    /// * `was_top` - Whether the bar sat along the top edge before.
    /// * `was_height` - The height it reserved before, margin included.
    /// * `was_autohide` - Whether it hid before.
    fn sync_panel(&mut self, was_top: bool, was_height: i32, was_autohide: bool) {
        if self.panel_top() != was_top
            || self.reserved_height() != was_height
            || self.config.panel_autohide != was_autohide
        {
            self.requests.push(self.panel_request());
            self.redraw_backgrounds();
        }
        if !self.config.panel_autohide {
            self.panel_hidden = false;
            self.hide_at = None;
        }
        crate::i18n::select(self.config.language.as_deref());
        ui::set_theme(self.config.theme);
        ui::set_style(self.config.window);
        self.fonts.clear_cache();
        self.menu.invalidate();
        self.launcher.invalidate();
        self.redraw_backgrounds();
        for window in &mut self.files.windows {
            window.dirty = true;
        }
        self.settings.dirty = true;
        self.start_icon = None;
        for toplevel in &mut self.toplevels {
            toplevel.icon = None;
        }
    }

    /// Opens a picker for the start button icon, starting in the pictures folder.
    pub fn pick_start_icon(&mut self) {
        self.close_popup();
        let start = self
            .files
            .places
            .iter()
            .find(|p| p.icon == "folder-pictures")
            .map(|p| p.path.clone());
        self.open_files(
            start,
            Some(files::Picker {
                target: files::PickTarget::StartIcon,
                mimes: vec!["image/png", "image/jpeg", "image/svg+xml"],
            }),
        );
    }

    /// Opens a file manager window.
    ///
    /// # Arguments
    /// * `dir` - The folder to show, or `None` for the home directory.
    /// * `picker` - What the window picks for, when it is a picker.
    ///
    /// # Returns
    /// The window's number.
    pub fn open_files(&mut self, dir: Option<std::path::PathBuf>, picker: Option<files::Picker>) -> u32 {
        let defaults = files::Defaults {
            view: if self.config.files_list {
                files::View::List
            } else {
                files::View::Icons
            },
            sort: match self.config.files_sort.as_str() {
                "size" => files::SortKey::Size,
                "modified" => files::SortKey::Modified,
                "type" => files::SortKey::Kind,
                _ => files::SortKey::Name,
            },
            show_hidden: self.config.files_hidden,
        };
        let id = self.files.open(dir, picker, defaults);
        let title = self.files.window(id).map(|w| w.title()).unwrap_or_default();
        let icon = self
            .icons
            .get("system-file-manager", 48)
            .map(|icon| Box::new((*icon).clone()));
        self.requests.push(Request::OpenWindow {
            id,
            title,
            width: files::DEFAULT_WIDTH,
            height: files::DEFAULT_HEIGHT,
            app_id: entries::FILES_APP_ID,
            icon_name: "system-file-manager",
            icon,
        });
        id
    }

    /// Opens a picker for a wallpaper image, starting in the pictures folder.
    ///
    /// # Arguments
    /// * `key` - The config key the chosen path is written to.
    /// * `monitor` - The monitor the settings window showed, restored afterwards.
    pub fn pick_wallpaper(&mut self, key: &'static str, monitor: Option<u32>) {
        self.close_popup();
        let start = self
            .files
            .places
            .iter()
            .find(|p| p.icon == "folder-pictures")
            .map(|p| p.path.clone());
        self.open_files(
            start,
            Some(files::Picker {
                target: files::PickTarget::Wallpaper { key, monitor },
                mimes: vec!["image/png", "image/jpeg"],
            }),
        );
    }

    /// The numbers of every window of the shell's own, the file manager's
    /// and the settings window, for rendering them in turn.
    pub fn window_ids(&self) -> Vec<u32> {
        let mut ids = self.files.ids();
        ids.extend(self.settings.id);
        ids
    }

    /// Whether a window number is the settings window's.
    fn is_settings(&self, id: u32) -> bool {
        self.settings.id == Some(id)
    }

    /// Opens the settings window, on a page when one is asked for, as a
    /// toplevel of the shell's own sized for the screen.  A window already
    /// open is closed first and opened again, since the shell cannot ask
    /// the window manager to raise one of its own windows.
    ///
    /// # Arguments
    /// * `page` - The page to show, or `None` to keep the last one.
    pub fn open_settings(&mut self, page: Option<Page>) {
        self.close_popup();
        if self.settings.id.is_some() {
            settings::close(self);
        }
        let id = self.files.allocate_id();
        self.settings.open(page);
        self.settings.id = Some(id);
        let screen_h = self
            .primary_background()
            .filter(|b| b.geometry.configured)
            .map_or(self.panel.height, |b| b.geometry.height);
        let width = settings::WIDTH
            .min(self.panel.width - 2 * ui::POPUP_MARGIN)
            .max(ui::WINDOW_MIN_WIDTH);
        let height = settings::HEIGHT
            .min(screen_h - self.reserved_height() - 2 * ui::POPUP_MARGIN)
            .max(ui::WINDOW_MIN_HEIGHT);
        let icon = self
            .icons
            .get("preferences-system", 48)
            .map(|icon| Box::new((*icon).clone()));
        self.requests.push(Request::OpenWindow {
            id,
            title: tr("Settings").to_string(),
            width,
            height,
            app_id: entries::SETTINGS_APP_ID,
            icon_name: "preferences-system",
            icon,
        });
    }

    /// Marks the settings window for redraw.
    pub fn redraw_settings(&mut self) {
        self.settings.dirty = true;
    }

    /// Reads the applications, icons and settings again.
    pub fn reload(&mut self) {
        let (was_top, was_height, was_autohide) = (
            self.panel_top(),
            self.reserved_height(),
            self.config.panel_autohide,
        );
        self.config = Config::load();
        self.sync_panel(was_top, was_height, was_autohide);
        crate::wmtheme::apply(&self.config);
        let decision = crate::toolkits::apply(&self.config);
        if let Some(portal) = self.portal.as_mut() {
            portal.update(&decision);
        }
        self.icons = Icons::new(self.config.icon_theme.as_deref());
        self.apps = Apps::load(entries::find_terminal(&self.config).is_some());
        self.wallpapers.clear();
        self.desktop_icons.clear();
        for toplevel in &mut self.toplevels {
            toplevel.icon = None;
        }
        self.menu.invalidate();
        self.launcher.invalidate();
        self.desktop_selection = None;
        self.panel_dirty = true;
        self.repaint_wallpapers();
    }

    /// Reads the settings again and applies what changed on screen, without
    /// rescanning the applications.
    pub fn apply_config(&mut self) {
        let (was_top, was_height, was_autohide) = (
            self.panel_top(),
            self.reserved_height(),
            self.config.panel_autohide,
        );
        self.config = Config::load();
        self.sync_panel(was_top, was_height, was_autohide);
        crate::wmtheme::apply(&self.config);
        let decision = crate::toolkits::apply(&self.config);
        if let Some(portal) = self.portal.as_mut() {
            portal.update(&decision);
        }
        self.wallpapers.retain(|_, image| image.is_some());
        self.repaint_wallpapers();
        self.panel_dirty = true;
    }

    /// A wallpaper image at its native size, decoded once per path.
    ///
    /// # Arguments
    /// * `path` - The image file.
    pub fn wallpaper_image(&mut self, path: &std::path::Path) -> Option<Arc<Pixmap>> {
        if let Some(image) = self.wallpapers.get(path) {
            return image.clone();
        }
        let image = crate::render::load_raster(path, 0).map(Arc::new);
        self.wallpapers.insert(path.to_path_buf(), image.clone());
        image
    }

    /// The monitors in the order the settings number them: the primary
    /// background's monitors first, then each other background's, as
    /// (background id, monitor index) pairs.
    pub fn monitor_list(&self) -> Vec<(u32, usize)> {
        let mut list = Vec::new();
        for background in self.backgrounds.iter().filter(|b| b.primary) {
            for index in 0..background.monitors.len().max(1) {
                list.push((background.id, index));
            }
        }
        for background in self.backgrounds.iter().filter(|b| !b.primary) {
            for index in 0..background.monitors.len().max(1) {
                list.push((background.id, index));
            }
        }
        list
    }

    /// The settings number of a monitor, starting at 1.
    ///
    /// # Arguments
    /// * `background` - The background surface.
    /// * `index` - The monitor's index within the surface.
    pub fn monitor_number(&self, background: u32, index: usize) -> u32 {
        self.monitor_list()
            .iter()
            .position(|(b, i)| *b == background && *i == index)
            .map_or(1, |p| p as u32 + 1)
    }

    /// The background surface with a backend number.
    ///
    /// # Arguments
    /// * `id` - The backend's number for the surface.
    pub fn background(&self, id: u32) -> Option<&BackgroundSurface> {
        self.backgrounds.iter().find(|b| b.id == id)
    }

    /// The background surface that carries the desktop icons.
    pub fn primary_background(&self) -> Option<&BackgroundSurface> {
        self.backgrounds.iter().find(|b| b.primary)
    }

    /// The numbers of every background surface, for rendering them in turn.
    pub fn background_ids(&self) -> Vec<u32> {
        self.backgrounds.iter().map(|b| b.id).collect()
    }

    /// Marks every background for redraw.
    fn redraw_backgrounds(&mut self) {
        for background in &mut self.backgrounds {
            background.dirty = true;
        }
    }

    /// Paints the wallpapers again from the settings, dropping the layers
    /// cached with the old color and image.
    pub fn repaint_wallpapers(&mut self) {
        for background in &mut self.backgrounds {
            background.wallpaper_cache = None;
            background.dirty = true;
        }
    }

    /// Takes the requests the handled events produced.
    pub fn take_requests(&mut self) -> Vec<Request> {
        std::mem::take(&mut self.requests)
    }

    /// Whether the bar sits along the top edge.
    pub fn panel_top(&self) -> bool {
        self.config.panel_position == crate::config::PanelPosition::Top
    }

    /// The bar's rectangle in panel surface coordinates: its width a share
    /// of the screen's, centered, and pulled in from the edges by its
    /// margin.
    pub fn bar_rect(&self) -> Rect {
        let height = self.config.panel_height;
        let margin = self.config.panel_margin;
        let usable = (self.panel.width - 2 * margin).max(1);
        let width = (usable * self.config.panel_width / 100).max(1);
        let x = margin + (usable - width) / 2;
        if self.panel_top() {
            return Rect::new(x, margin, width, height);
        }
        Rect::new(x, self.panel.height - height - margin, width, height)
    }

    /// Whether the window at an index exists.
    fn toplevel_index(&self, id: u64) -> Option<usize> {
        self.toplevels.iter().position(|t| t.id == id)
    }

    /// The themed or self-published icon of a window at the current scale.
    ///
    /// # Arguments
    /// * `index` - Index into the window list.
    pub fn toplevel_icon(&mut self, index: usize) -> Option<Arc<Pixmap>> {
        let scale = self.panel.scale.max(1);
        if let Some((cached_scale, icon)) = &self.toplevels[index].icon
            && *cached_scale == scale
        {
            return icon.clone();
        }
        let size = (self.panel_icon_size() * scale) as u32;
        let app_id = self.toplevels[index].app_id.clone();
        let mut icon = if app_id.is_empty() {
            None
        } else {
            self.icons.get(&app_id, size)
        };
        if icon.is_none()
            && !app_id.is_empty()
            && let Some(name) = entries::icon_for_app_id(&self.apps.entries, &app_id)
                .or_else(|| entries::load_entry(&format!("{app_id}.desktop")).and_then(|e| e.icon))
        {
            icon = self.icons.get(&name, size);
        }
        if icon.is_none() {
            let lower = app_id.to_lowercase();
            let short = lower.rsplit('.').next().unwrap_or("").to_string();
            if !short.is_empty() && short != lower {
                icon = self.icons.get(&short, size);
            }
        }
        if icon.is_none()
            && let Some(own) = &self.toplevels[index].own_icon
        {
            icon = crate::render::resize_pixmap(own, size).map(Arc::new);
        }
        self.toplevels[index].icon = Some((scale, icon.clone()));
        icon
    }

    /// The icon of a desktop launcher at the current scale.
    ///
    /// # Arguments
    /// * `index` - Index into the desktop launcher list.
    pub fn desktop_icon(&mut self, index: usize) -> Option<Arc<Pixmap>> {
        let scale = self.primary_background().map_or(1, |b| b.geometry.scale).max(1);
        if let Some(icon) = self.desktop_icons.get(&(index, scale)) {
            return icon.clone();
        }
        let name = self.apps.desktop.get(index).and_then(|e| e.icon.clone());
        let icon = name.and_then(|n| self.icons.get(&n, (ui::DESKTOP_ICON_SIZE * scale) as u32));
        self.desktop_icons.insert((index, scale), icon.clone());
        icon
    }

    /// The icon of an application at the menu's icon size.
    ///
    /// # Arguments
    /// * `index` - Index into the application list.
    pub fn app_icon(&mut self, index: usize) -> Option<Arc<Pixmap>> {
        self.app_icon_sized(index, ui::ICON_SIZE)
    }

    /// The icon of an application at a logical size.
    ///
    /// # Arguments
    /// * `index` - Index into the application list.
    /// * `size` - Logical size wanted.
    pub fn app_icon_sized(&mut self, index: usize, size: i32) -> Option<Arc<Pixmap>> {
        let scale = self.panel.scale.max(1);
        let name = self.apps.entries.get(index).and_then(|e| e.icon.clone())?;
        self.icons.get(&name, (size * scale) as u32)
    }

    /// Writes a setting and applies it, the way the settings window does.
    ///
    /// # Arguments
    /// * `changes` - Keys and their new values; an empty value removes the key.
    pub fn write_config(&mut self, changes: &[(&str, String)]) {
        if let Err(error) = crate::config::save(changes) {
            eprintln!("selkies-desktop: cannot write the config file: {error}");
        }
        self.apply_config();
    }

    /// Adds a desktop file ID to a list setting, or removes it when present.
    ///
    /// # Arguments
    /// * `key` - The setting: `favorites` or `launchers`.
    /// * `id` - The desktop file ID.
    pub fn toggle_list_entry(&mut self, key: &'static str, id: &str) {
        let mut list = if key == "favorites" {
            self.config.favorites.clone()
        } else {
            self.config.launchers.clone()
        };
        if let Some(pos) = list.iter().position(|e| e == id) {
            list.remove(pos);
        } else {
            list.push(id.to_string());
        }
        let value = if list.is_empty() && key == "favorites" {
            "none".to_string()
        } else {
            list.join(", ")
        };
        self.write_config(&[(key, value)]);
        self.menu.invalidate();
        self.launcher.invalidate();
    }

    /// Handles one event from the backend.
    ///
    /// # Arguments
    /// * `event` - The event.
    pub fn handle(&mut self, event: Event) {
        match event {
            Event::Configured {
                surface,
                width,
                height,
                scale,
            } => self.configure(surface, width, height, scale),
            Event::BackgroundLayout {
                surface,
                monitors,
                primary,
            } => {
                if primary {
                    for background in &mut self.backgrounds {
                        if background.id != surface && background.primary {
                            background.primary = false;
                            background.dirty = true;
                        }
                    }
                }
                if let Some(background) = self.backgrounds.iter_mut().find(|b| b.id == surface)
                    && (background.monitors != monitors || background.primary != primary)
                {
                    background.monitors = monitors;
                    background.primary = primary;
                    background.dirty = true;
                }
            }
            Event::BackgroundRemoved { surface } => {
                self.backgrounds.retain(|b| b.id != surface);
            }
            Event::PointerMotion { surface, x, y } => self.pointer_motion(surface, x, y),
            Event::PointerButton {
                surface,
                x,
                y,
                button,
                pressed,
                time,
                modifiers,
            } => self.pointer_button(surface, x, y, button, pressed, time, modifiers),
            Event::PointerAxis { surface, x, y, delta } => self.pointer_axis(surface, x, y, delta),
            Event::PointerLeave { surface } => self.pointer_leave(surface),
            Event::Key {
                surface,
                key,
                modifiers,
            } => match surface {
                SurfaceId::Window(id) if self.is_settings(id) => settings::key(self, key),
                SurfaceId::Window(id) => ui::files::key(self, id, key, modifiers),
                _ => self.key(key),
            },
            Event::KeyboardLeave { surface } => {
                if surface == SurfaceId::Panel && matches!(self.popup, Popup::Menu | Popup::Launcher) {
                    self.close_popup();
                }
            }
            Event::WindowCloseRequested { window } => {
                if self.is_settings(window) {
                    settings::close(self);
                } else {
                    ui::files::close(self, window);
                }
            }
            Event::DragEnter { surface, x, y } | Event::DragMotion { surface, x, y } => {
                if let SurfaceId::Window(id) = surface {
                    ui::files::drag_motion(self, id, x, y);
                }
            }
            Event::DragLeave { surface } => {
                if let SurfaceId::Window(id) = surface {
                    ui::files::drag_leave(self, id);
                }
            }
            Event::DragDrop { surface, x, y, copy } => match surface {
                SurfaceId::Window(id) => {
                    ui::files::drag_motion(self, id, x, y);
                    ui::files::drop(self, id, copy);
                }
                SurfaceId::Background(_) => ui::files::drop_on_desktop(self, copy),
                SurfaceId::Panel => {}
            },
            Event::DragEnd => {
                self.files.drag = None;
                for window in &mut self.files.windows {
                    if window.drop_target.take().is_some() {
                        window.dirty = true;
                    }
                }
            }
            Event::ToplevelAdded { id } => {
                if self.toplevel_index(id).is_none() {
                    self.toplevels.push(Toplevel {
                        id,
                        title: String::new(),
                        app_id: String::new(),
                        own_icon: None,
                        state: ToplevelState::default(),
                        icon: None,
                    });
                    self.panel_dirty = true;
                }
            }
            Event::ToplevelTitle { id, title } => {
                if let Some(i) = self.toplevel_index(id) {
                    self.toplevels[i].title = title;
                    self.panel_dirty = true;
                }
            }
            Event::ToplevelAppId { id, app_id } => {
                if let Some(i) = self.toplevel_index(id) {
                    self.toplevels[i].app_id = app_id;
                    self.toplevels[i].icon = None;
                    self.panel_dirty = true;
                }
            }
            Event::ToplevelIcon { id, icon } => {
                if let Some(i) = self.toplevel_index(id) {
                    self.toplevels[i].own_icon = Some(*icon);
                    self.toplevels[i].icon = None;
                    self.panel_dirty = true;
                }
            }
            Event::ToplevelState { id, state } => {
                if let Some(i) = self.toplevel_index(id)
                    && self.toplevels[i].state != state
                {
                    self.toplevels[i].state = state;
                    self.panel_dirty = true;
                }
            }
            Event::ToplevelClosed { id } => {
                if let Some(i) = self.toplevel_index(id) {
                    self.toplevels.remove(i);
                    self.shown_desktop.retain(|w| *w != id);
                    if let Popup::TaskMenu(t) = self.popup
                        && t >= i
                    {
                        self.close_popup();
                    }
                    if let Hover::Task(t) = self.hover
                        && t >= i
                    {
                        self.set_hover(Hover::None);
                    }
                    self.panel_dirty = true;
                }
            }
            Event::Desktops { count, current } => {
                let count = count.max(1);
                let current = current.min(count - 1);
                if (self.desktops, self.current_desktop) != (count, current) {
                    self.desktops = count;
                    self.current_desktop = current;
                    if let Hover::Pager(_) = self.hover {
                        self.set_hover(Hover::None);
                    }
                    self.panel_dirty = true;
                }
            }
            Event::Quit => {}
        }
    }

    /// Records the size of a surface and schedules its redraw.  A background
    /// surface seen for the first time is created, primary when it is the
    /// first one.
    fn configure(&mut self, surface: SurfaceId, width: i32, height: i32, scale: i32) {
        let scale = scale.max(1);
        let geometry = Geometry {
            width,
            height,
            scale,
            configured: width > 0 && height > 0,
        };
        match surface {
            SurfaceId::Panel => {
                let current = self.panel;
                if current.width == width
                    && current.height == height
                    && current.scale == scale
                    && current.configured
                {
                    return;
                }
                if self.panel.scale != scale {
                    self.fonts.clear_cache();
                    self.menu.invalidate();
                }
                self.panel = geometry;
                self.panel_pix = None;
                self.panel_visible.clear();
                self.panel_dirty = true;
            }
            SurfaceId::Background(id) => {
                let first = self.backgrounds.is_empty();
                let Some(background) = self.backgrounds.iter_mut().find(|b| b.id == id) else {
                    self.backgrounds.push(BackgroundSurface {
                        id,
                        geometry,
                        monitors: vec![Rect::new(0, 0, width, height)],
                        primary: first,
                        pix: None,
                        launchers_pix: None,
                        dirty: true,
                        wallpaper_cache: None,
                    });
                    return;
                };
                let current = background.geometry;
                if current.width == width
                    && current.height == height
                    && current.scale == scale
                    && current.configured
                {
                    return;
                }
                background.geometry = geometry;
                if background.monitors.len() <= 1 {
                    background.monitors = vec![Rect::new(0, 0, width, height)];
                }
                background.pix = None;
                background.launchers_pix = None;
                background.dirty = true;
            }
            SurfaceId::Window(id) if self.is_settings(id) => {
                let current = self.settings.geometry;
                if current.width == width
                    && current.height == height
                    && current.scale == scale
                    && current.configured
                {
                    return;
                }
                self.settings.geometry = geometry;
                self.settings.pix = None;
                self.settings.dirty = true;
            }
            SurfaceId::Window(id) => {
                if let Some(window) = self.files.window_mut(id) {
                    let current = window.geometry;
                    if current.width == width
                        && current.height == height
                        && current.scale == scale
                        && current.configured
                    {
                        return;
                    }
                    window.geometry = geometry;
                    window.pix = None;
                    window.dirty = true;
                }
            }
        }
    }

    /// Updates the clock text and tooltip timers.
    ///
    /// # Arguments
    /// * `now` - The current time.
    pub fn tick(&mut self, now: Instant) {
        let clock = if self.config.show_clock {
            crate::clock::format_now(&self.config.clock_format)
        } else {
            String::new()
        };
        if clock != self.clock {
            self.clock = clock;
            self.panel_dirty = true;
        }
        if self.preview.as_ref().is_some_and(|(_, _, at)| now >= *at) {
            self.apply_preview();
        }
        if let Some(at) = self.hide_at
            && now >= at
        {
            self.hide_at = None;
            if self.config.panel_autohide
                && self.popup == Popup::None
                && !self.bar_rect().contains(self.pointer.0, self.pointer.1)
            {
                self.panel_hidden = true;
                self.panel_dirty = true;
            }
        }
        if self.tooltip.is_none()
            && self.popup == Popup::None
            && now.duration_since(self.hover_since) >= Duration::from_millis(ui::TOOLTIP_DELAY_MS)
            && let Some(text) = self.tooltip_for(self.hover)
        {
            self.tooltip = Some(text);
            self.panel_dirty = true;
        }
        self.files.tick(now);
    }

    /// When `tick` next has something to do.
    ///
    /// # Arguments
    /// * `now` - The current time.
    pub fn next_deadline(&self, now: Instant) -> Option<Instant> {
        let mut deadline = if self.config.show_clock {
            Some(crate::clock::next_minute(now))
        } else {
            None
        };
        if self.tooltip.is_none() && self.popup == Popup::None && self.tooltip_for(self.hover).is_some() {
            let due = self.hover_since + Duration::from_millis(ui::TOOLTIP_DELAY_MS);
            deadline = Some(deadline.map_or(due, |d: Instant| d.min(due)));
        }
        if let Some(due) = self.files.next_deadline() {
            deadline = Some(deadline.map_or(due, |d: Instant| d.min(due)));
        }
        if let Some(due) = self.hide_at {
            deadline = Some(deadline.map_or(due, |d: Instant| d.min(due)));
        }
        if let Some((_, _, due)) = &self.preview {
            let due = *due;
            deadline = Some(deadline.map_or(due, |d: Instant| d.min(due)));
        }
        deadline
    }

    /// The tooltip for a hover target.
    fn tooltip_for(&self, hover: Hover) -> Option<String> {
        match hover {
            Hover::Task(i) => self.toplevels.get(i).map(|t| t.label().to_string()),
            Hover::Volume => self.volume.map(|v| {
                if v.muted {
                    tr("Muted").to_string()
                } else {
                    trf("Volume {n}%", &[("n", &v.percent.to_string())])
                }
            }),
            Hover::Clock => Some(crate::clock::format_now("%A, %B %-d, %Y")),
            Hover::ShowDesktop => Some(tr("Show desktop").to_string()),
            Hover::Pager(i) => Some(trf("Desktop {n}", &[("n", &(i + 1).to_string())])),
            Hover::Start => Some(tr("Applications (Super: quick launcher)").to_string()),
            Hover::Launcher(i) => self
                .config
                .launchers
                .get(i)
                .and_then(|id| self.apps.find(id))
                .map(|index| self.apps.entries[index].name.clone()),
            Hover::MenuApp(i) => {
                let entry = self.menu.visible_entry(self, i)?;
                let entry = &self.apps.entries[entry];
                entry.comment.clone().filter(|c| c != &entry.name)
            }
            Hover::MenuTile(i) => {
                let entry = *menu::favorite_entries(self).get(i)?;
                let entry = &self.apps.entries[entry];
                entry.comment.clone().filter(|c| c != &entry.name)
            }
            _ => None,
        }
    }

    /// Changes the hover target, restarting the tooltip timer.
    pub fn set_hover(&mut self, hover: Hover) {
        if self.hover != hover {
            if matches!(hover, Hover::DesktopIcon(_)) || matches!(self.hover, Hover::DesktopIcon(_)) {
                self.redraw_backgrounds();
            }
            for previous in [self.hover, hover] {
                if let Hover::Files(id, _) = previous
                    && let Some(window) = self.files.window_mut(id)
                {
                    window.dirty = true;
                }
                if let Hover::Control(_) = previous {
                    self.settings.dirty = true;
                }
            }
            let panel_changed = !matches!(
                (self.hover, hover),
                (
                    Hover::Files(..) | Hover::Control(_) | Hover::None,
                    Hover::Files(..) | Hover::Control(_) | Hover::None
                )
            );
            self.hover = hover;
            self.hover_since = Instant::now();
            if self.tooltip.take().is_some() || panel_changed {
                self.panel_dirty = true;
            }
        }
        let pointer = match hover {
            Hover::None => false,
            Hover::Files(_, hit) => hit.clickable(),
            _ => true,
        };
        if pointer != self.cursor_pointer {
            self.cursor_pointer = pointer;
            self.requests.push(Request::Cursor { pointer });
        }
    }

    /// Sets the volume the audio module reports.
    ///
    /// # Arguments
    /// * `volume` - The new volume, or `None` when audio is unavailable.
    pub fn set_volume(&mut self, volume: Option<Volume>) {
        if self.volume != volume {
            self.volume = volume;
            if volume.is_none() && self.popup == Popup::Volume {
                self.close_popup();
            }
            self.panel_dirty = true;
        }
    }

    /// Opens a popup, closing whatever was open.
    ///
    /// # Arguments
    /// * `popup` - The popup to open.
    pub fn open_popup(&mut self, popup: Popup) {
        if self.popup == popup {
            return;
        }
        let was_open = self.popup != Popup::None;
        self.popup = popup;
        self.tooltip = None;
        if popup == Popup::Menu {
            let mut menu = std::mem::take(&mut self.menu);
            menu.open(self);
            self.menu = menu;
        }
        if popup == Popup::Launcher {
            self.launcher.open();
        }
        if popup == Popup::Calendar {
            self.calendar_offset = 0;
        }
        if !was_open {
            self.requests.push(Request::MenuOpen(true));
        }
        self.panel_dirty = true;
    }

    /// Closes the open popup.
    pub fn close_popup(&mut self) {
        if self.popup == Popup::None {
            return;
        }
        self.popup = Popup::None;
        self.volume_drag = false;
        self.menu.close();
        self.launcher.close();
        self.requests.push(Request::MenuOpen(false));
        self.set_hover(Hover::None);
        self.panel_dirty = true;
        if !self.bar_rect().contains(self.pointer.0, self.pointer.1) {
            self.schedule_hide();
        }
    }

    /// Toggles the start menu, from a click on the start button.
    pub fn toggle_menu(&mut self) {
        if self.popup == Popup::Menu {
            self.close_popup();
        } else {
            self.open_popup(Popup::Menu);
        }
    }

    /// Toggles the quick launcher, from the `--launcher` command the window
    /// manager sends on a release of the Super key.
    pub fn toggle_launcher(&mut self) {
        if self.popup == Popup::Launcher {
            self.close_popup();
        } else {
            self.open_popup(Popup::Launcher);
        }
    }

    /// Launches an application and closes the menu.
    ///
    /// # Arguments
    /// * `index` - Index into the application list.
    pub fn launch_app(&mut self, index: usize) {
        if let Some(entry) = self.apps.entries.get(index) {
            entries::launch(entry, &self.config);
        }
        self.close_popup();
    }

    /// Acts on a window.
    ///
    /// # Arguments
    /// * `index` - Index into the window list.
    /// * `action` - What to do.
    pub fn window_action(&mut self, index: usize, action: WindowAction) {
        if let Some(toplevel) = self.toplevels.get(index) {
            self.requests.push(Request::Window {
                id: toplevel.id,
                action,
            });
        }
    }

    /// Activates a window, restoring it when it is minimized, or minimizes it
    /// when it is already active.
    ///
    /// # Arguments
    /// * `index` - Index into the window list.
    pub fn task_click(&mut self, index: usize) {
        let Some(toplevel) = self.toplevels.get(index) else {
            return;
        };
        if toplevel.state.active && !toplevel.state.minimized {
            self.window_action(index, WindowAction::Minimize);
        } else {
            if toplevel.state.minimized {
                self.window_action(index, WindowAction::Unminimize);
            }
            self.window_action(index, WindowAction::Activate);
        }
        self.shown_desktop.clear();
    }

    /// Asks the window manager for a virtual desktop.
    ///
    /// # Arguments
    /// * `index` - The desktop, from 0; the current one or one past the
    ///   last is ignored.
    pub fn switch_desktop(&mut self, index: u32) {
        if index < self.desktops && index != self.current_desktop {
            self.requests.push(Request::SwitchDesktop(index));
        }
    }

    /// Whether a window is listed on the taskbar: one on the current desktop,
    /// or any when the taskbar shows every desktop.
    ///
    /// # Arguments
    /// * `index` - Index into the window list.
    pub fn task_listed(&self, index: usize) -> bool {
        self.config.tasks_all_desktops || !self.toplevels[index].state.hidden
    }

    /// Minimizes every window of the current desktop, or restores the ones
    /// the previous press hid.
    pub fn show_desktop(&mut self) {
        if !self.shown_desktop.is_empty() {
            let ids = std::mem::take(&mut self.shown_desktop);
            for id in ids {
                if let Some(i) = self.toplevel_index(id) {
                    self.window_action(i, WindowAction::Unminimize);
                }
            }
            return;
        }
        let visible: Vec<usize> = self
            .toplevels
            .iter()
            .enumerate()
            .filter(|(_, t)| !t.state.minimized && !t.state.hidden)
            .map(|(i, _)| i)
            .collect();
        for i in visible {
            self.shown_desktop.push(self.toplevels[i].id);
            self.window_action(i, WindowAction::Minimize);
        }
    }

    /// Changes the volume by a number of percent points.
    ///
    /// # Arguments
    /// * `delta` - Points to add, negative to lower.
    pub fn adjust_volume(&mut self, delta: i32) {
        if let Some(volume) = self.volume {
            let target = (volume.percent as i32 + delta).clamp(0, 150) as u8;
            self.requests.push(Request::SetVolume(target));
        }
    }

    /// Sets the volume to an absolute percentage.
    ///
    /// # Arguments
    /// * `percent` - The level.
    pub fn set_volume_target(&mut self, percent: u8) {
        if self.volume.is_some() {
            self.requests.push(Request::SetVolume(percent.min(150)));
        }
    }

    /// Toggles mute.
    pub fn toggle_mute(&mut self) {
        if self.volume.is_some() {
            self.requests.push(Request::ToggleMute);
        }
    }

    /// Routes pointer motion to the surface's UI.
    fn pointer_motion(&mut self, surface: SurfaceId, x: f64, y: f64) {
        match surface {
            SurfaceId::Panel => {
                self.pointer = (x, y);
                if self.config.panel_autohide {
                    self.reveal_panel();
                }
                if self.volume_drag {
                    popups::volume_drag(self, x);
                    return;
                }
                let hover = match self.popup {
                    Popup::Menu => menu::hit(self, x, y).unwrap_or(Hover::None),
                    Popup::Launcher => launcher::hit(self, x, y).unwrap_or(Hover::None),
                    Popup::TaskMenu(_) => popups::task_menu_hit(self, x, y).unwrap_or(Hover::None),
                    Popup::PanelMenu(..) => settings::panel_menu_hit(self, x, y).unwrap_or(Hover::None),
                    _ => Hover::None,
                };
                let hover = if hover == Hover::None {
                    panel::hit(self, x, y)
                } else {
                    hover
                };
                self.set_hover(hover);
            }
            SurfaceId::Background(id) => {
                let hover = background::hit(self, id, x, y)
                    .map(Hover::DesktopIcon)
                    .unwrap_or(Hover::None);
                if hover != self.hover {
                    self.set_hover(hover);
                    self.panel_dirty = false;
                }
            }
            SurfaceId::Window(id) if self.is_settings(id) => settings::pointer_motion(self, x, y),
            SurfaceId::Window(id) => ui::files::pointer_motion(self, id, x, y),
        }
    }

    /// Routes a button press or release.
    #[allow(clippy::too_many_arguments)]
    fn pointer_button(
        &mut self,
        surface: SurfaceId,
        x: f64,
        y: f64,
        button: Button,
        pressed: bool,
        time: u32,
        modifiers: Modifiers,
    ) {
        if let SurfaceId::Window(id) = surface {
            if self.is_settings(id) {
                settings::pointer_button(self, x, y, button, pressed);
            } else {
                ui::files::pointer_button(self, id, x, y, button, pressed, time, modifiers);
            }
            return;
        }
        if !pressed {
            if self.volume_drag {
                self.volume_drag = false;
            }
            return;
        }
        match surface {
            SurfaceId::Window(_) => {}
            SurfaceId::Background(id) => self.background_click(id, x, y, button, time),
            SurfaceId::Panel => {
                self.pointer = (x, y);
                if self.popup != Popup::None {
                    let inside_popup = match self.popup {
                        Popup::Menu => menu::click(self, x, y, button),
                        Popup::Launcher => launcher::click(self, x, y, button),
                        Popup::Volume => popups::volume_click(self, x, y, button),
                        Popup::Calendar => popups::calendar_click(self, x, y, button),
                        Popup::TaskMenu(_) => popups::task_menu_click(self, x, y, button),
                        Popup::PanelMenu(..) => settings::panel_menu_click(self, x, y, button),
                        Popup::None => false,
                    };
                    if inside_popup {
                        return;
                    }
                    let was = self.popup;
                    self.close_popup();
                    if !self.bar_rect().contains(x, y) {
                        return;
                    }
                    if let Hover::Start = panel::hit(self, x, y)
                        && was == Popup::Menu
                    {
                        return;
                    }
                    if let Hover::Volume = panel::hit(self, x, y)
                        && was == Popup::Volume
                        && button == Button::Left
                    {
                        return;
                    }
                    if let Hover::Clock = panel::hit(self, x, y)
                        && was == Popup::Calendar
                        && button == Button::Left
                    {
                        return;
                    }
                }
                panel::click(self, x, y, button, time);
            }
        }
    }

    /// Handles a click on a background: selects or launches a desktop icon.
    fn background_click(&mut self, id: u32, x: f64, y: f64, button: Button, time: u32) {
        if self.popup != Popup::None {
            self.close_popup();
        }
        let hit = background::hit(self, id, x, y);
        match hit {
            Some(index) if button == Button::Left => {
                let double = self
                    .last_click
                    .is_some_and(|(t, i)| i == index && time.wrapping_sub(t) < ui::DOUBLE_CLICK_MS);
                if double {
                    if let Some(entry) = self.apps.desktop.get(index) {
                        entries::launch(entry, &self.config);
                    }
                    self.last_click = None;
                    self.desktop_selection = None;
                } else {
                    self.last_click = Some((time, index));
                    self.desktop_selection = Some(index);
                }
                self.redraw_backgrounds();
            }
            Some(index) => {
                self.desktop_selection = Some(index);
                self.last_click = None;
                self.redraw_backgrounds();
            }
            None => {
                self.last_click = None;
                if self.desktop_selection.take().is_some() {
                    self.redraw_backgrounds();
                }
            }
        }
    }

    /// Routes wheel scrolling.
    fn pointer_axis(&mut self, surface: SurfaceId, x: f64, y: f64, delta: f64) {
        if let SurfaceId::Window(id) = surface {
            if self.is_settings(id) {
                settings::pointer_axis(self, x, y, delta);
            } else {
                ui::files::pointer_axis(self, id, x, y, delta);
            }
            return;
        }
        if surface != SurfaceId::Panel {
            return;
        }
        self.pointer = (x, y);
        let handled = match self.popup {
            Popup::Menu => menu::scroll(self, x, y, delta),
            Popup::Launcher => launcher::scroll(self, x, y, delta),
            Popup::Volume => popups::volume_scroll(self, x, y, delta),
            Popup::Calendar => popups::calendar_scroll(self, x, y, delta),
            _ => false,
        };
        if !handled {
            panel::scroll(self, x, y, delta);
        }
    }

    /// Clears hover state when the pointer leaves a surface.
    fn pointer_leave(&mut self, surface: SurfaceId) {
        match surface {
            SurfaceId::Panel => {
                self.pointer = (-1.0, -1.0);
                self.schedule_hide();
                if self.popup == Popup::None
                    || !matches!(
                        self.hover,
                        Hover::MenuApp(_) | Hover::MenuTile(_) | Hover::MenuCategory(_)
                    )
                {
                    self.set_hover(Hover::None);
                }
            }
            SurfaceId::Background(_) => {
                if matches!(self.hover, Hover::DesktopIcon(_)) {
                    self.set_hover(Hover::None);
                }
            }
            SurfaceId::Window(id) if self.is_settings(id) => settings::pointer_leave(self),
            SurfaceId::Window(id) => ui::files::pointer_leave(self, id),
        }
    }

    /// Routes a key press to the open popup.  The Super key is the window
    /// manager's: it runs `selkies-desktop --launcher` on the release, so a
    /// press that reaches the panel does nothing here.
    fn key(&mut self, key: Key) {
        match key {
            Key::Super => {}
            Key::Escape => self.close_popup(),
            _ => match self.popup {
                Popup::Menu => menu::key(self, key),
                Popup::Launcher => launcher::key(self, key),
                Popup::Calendar => popups::calendar_key(self, key),
                Popup::Volume => popups::volume_key(self, key),
                _ => {}
            },
        }
    }

    /// Marks the panel for redraw.
    pub fn redraw_panel(&mut self) {
        self.panel_dirty = true;
    }

    /// Tells the backend where the bar shows each window when that changed, so a minimize
    /// shrinks toward the window's own button.
    fn publish_task_rects(&mut self) {
        let layout = ui::panel::layout(self);
        let rects: Vec<(u64, Rect)> = layout
            .tasks
            .iter()
            .filter_map(|(index, rect)| self.toplevels.get(*index).map(|t| (t.id, *rect)))
            .collect();
        if rects != self.published_tasks {
            self.published_tasks = rects.clone();
            self.requests.push(Request::TaskbarRects(rects));
        }
    }

    /// Renders the panel when it changed.
    ///
    /// # Returns
    /// The frame to present, or `None` when nothing changed.
    pub fn render_panel(&mut self) -> Option<Frame<'_>> {
        if !self.panel.configured || !self.panel_dirty {
            return None;
        }
        self.panel_dirty = false;
        let geometry = self.panel;
        let (w, h) = (
            (geometry.width * geometry.scale) as u32,
            (geometry.height * geometry.scale) as u32,
        );
        let mut pix = match self.panel_pix.take() {
            Some(pix) if pix.width() == w && pix.height() == h => pix,
            _ => Pixmap::new(w, h)?,
        };
        let previous = std::mem::take(&mut self.panel_visible);
        let hidden = self.panel_hidden();
        let mut visible = vec![if hidden {
            self.reveal_strip()
        } else {
            self.bar_rect()
        }];
        {
            let mut canvas = Canvas::new(pix.as_mut(), geometry.scale);
            let clear = previous.iter().fold(self.bar_rect(), |acc, r| acc.union(r));
            canvas.clear(clear.inflate(CLEAR_MARGIN));
            if hidden {
                canvas.fill_rect(self.reveal_strip(), ui::panel_bg(self.config.panel_opacity / 2));
            } else {
                panel::draw(self, &mut canvas);
                visible.extend(popups::draw(self, &mut canvas));
            }
        }
        if self.opaque_output {
            self.flatten_over_wallpaper(&mut pix, &visible);
        }
        let mut damage: Vec<Rect> = previous.iter().map(|r| r.inflate(CLEAR_MARGIN)).collect();
        damage.extend(visible.iter().map(|r| r.inflate(CLEAR_MARGIN)));
        let grab = self.popup != Popup::None;
        let input = if grab {
            vec![Rect::new(0, 0, geometry.width, geometry.height)]
        } else {
            visible.clone()
        };
        self.panel_visible = visible.clone();
        self.panel_pix = Some(pix);
        self.publish_task_rects();
        Some(Frame {
            pixmap: self.panel_pix.as_ref()?,
            scale: geometry.scale,
            damage,
            input,
            visible,
            grab,
        })
    }

    /// Blends the panel's pixels over the wallpaper behind them and makes
    /// them opaque, for a display that drops alpha.  Pixels the wallpaper
    /// does not reach are left as they are.
    ///
    /// # Arguments
    /// * `pix` - The panel pixmap, premultiplied.
    /// * `rects` - The logical rectangles to blend.
    fn flatten_over_wallpaper(&self, pix: &mut Pixmap, rects: &[Rect]) {
        let Some((_, layer)) = self.primary_background().and_then(|b| b.wallpaper_cache.as_ref()) else {
            return;
        };
        let scale = self.panel.scale.max(1);
        let (ox, oy) = (self.panel_offset.0 * scale, self.panel_offset.1 * scale);
        let (pw, ph) = (pix.width() as i32, pix.height() as i32);
        let (lw, lh) = (layer.width() as i32, layer.height() as i32);
        let layer_data = layer.data();
        let data = pix.data_mut();
        for rect in rects {
            let r = rect.scaled(scale).intersect(&Rect::new(0, 0, pw, ph));
            for y in r.y..r.bottom() {
                let by = y + oy;
                if by < 0 || by >= lh {
                    continue;
                }
                for x in r.x..r.right() {
                    let bx = x + ox;
                    if bx < 0 || bx >= lw {
                        continue;
                    }
                    let d = (y as usize * pw as usize + x as usize) * 4;
                    let s = (by as usize * lw as usize + bx as usize) * 4;
                    let a = data[d + 3] as u32;
                    if a == 255 {
                        continue;
                    }
                    let inv = 255 - a;
                    for c in 0..3 {
                        data[d + c] =
                            (data[d + c] as u32 + layer_data[s + c] as u32 * inv / 255).min(255) as u8;
                    }
                    data[d + 3] = 255;
                }
            }
        }
    }

    /// Renders a background surface when it changed.
    ///
    /// # Arguments
    /// * `id` - The backend's number for the surface.
    ///
    /// # Returns
    /// The frame to present, or `None` when nothing changed.
    pub fn render_background(&mut self, id: u32) -> Option<Frame<'_>> {
        let index = self.backgrounds.iter().position(|b| b.id == id)?;
        let geometry = self.backgrounds[index].geometry;
        if !geometry.configured || !self.backgrounds[index].dirty {
            return None;
        }
        self.backgrounds[index].dirty = false;
        let (w, h) = (
            (geometry.width * geometry.scale) as u32,
            (geometry.height * geometry.scale) as u32,
        );
        let mut pix = match self.backgrounds[index].pix.take() {
            Some(pix) if pix.width() == w && pix.height() == h => pix,
            _ => Pixmap::new(w, h)?,
        };
        {
            let mut canvas = Canvas::new(pix.as_mut(), geometry.scale);
            background::draw(self, id, &mut canvas);
        }
        let whole = Rect::new(0, 0, geometry.width, geometry.height);
        let input = if self.backgrounds[index].primary {
            background::cells(self)
        } else {
            Vec::new()
        };
        self.backgrounds[index].pix = Some(pix);
        Some(Frame {
            pixmap: self.backgrounds[index].pix.as_ref()?,
            scale: geometry.scale,
            damage: vec![whole],
            input,
            visible: vec![whole],
            grab: false,
        })
    }

    /// Renders a background surface as two layers when it changed: the
    /// wallpaper alone, and the launchers over a transparent canvas, for a
    /// backend that shows them above the wallpaper layers of other programs.
    ///
    /// # Arguments
    /// * `id` - The background surface.
    ///
    /// # Returns
    /// The wallpaper frame, which takes no input, and the launchers frame,
    /// whose input is the launcher cells; `None` when nothing changed.
    pub fn render_background_layers(&mut self, id: u32) -> Option<(Frame<'_>, Frame<'_>)> {
        let index = self.backgrounds.iter().position(|b| b.id == id)?;
        let geometry = self.backgrounds[index].geometry;
        if !geometry.configured || !self.backgrounds[index].dirty {
            return None;
        }
        self.backgrounds[index].dirty = false;
        let (w, h) = (
            (geometry.width * geometry.scale) as u32,
            (geometry.height * geometry.scale) as u32,
        );
        let mut pix = match self.backgrounds[index].pix.take() {
            Some(pix) if pix.width() == w && pix.height() == h => pix,
            _ => Pixmap::new(w, h)?,
        };
        let mut launchers = match self.backgrounds[index].launchers_pix.take() {
            Some(pix) if pix.width() == w && pix.height() == h => pix,
            _ => Pixmap::new(w, h)?,
        };
        launchers.fill(tiny_skia::Color::TRANSPARENT);
        {
            let mut canvas = Canvas::new(pix.as_mut(), geometry.scale);
            background::draw_wallpaper_layer(self, id, &mut canvas);
        }
        {
            let mut canvas = Canvas::new(launchers.as_mut(), geometry.scale);
            background::draw_launchers(self, id, &mut canvas);
        }
        let whole = Rect::new(0, 0, geometry.width, geometry.height);
        let cells = if self.backgrounds[index].primary {
            background::cells(self)
        } else {
            Vec::new()
        };
        self.backgrounds[index].pix = Some(pix);
        self.backgrounds[index].launchers_pix = Some(launchers);
        let surface = &self.backgrounds[index];
        Some((
            Frame {
                pixmap: surface.pix.as_ref()?,
                scale: geometry.scale,
                damage: vec![whole],
                input: Vec::new(),
                visible: vec![whole],
                grab: false,
            },
            Frame {
                pixmap: surface.launchers_pix.as_ref()?,
                scale: geometry.scale,
                damage: vec![whole],
                input: cells.clone(),
                visible: cells,
                grab: false,
            },
        ))
    }

    /// Renders one of the shell's windows when it changed: the settings
    /// window, or a file manager window.
    ///
    /// # Arguments
    /// * `id` - The window's number.
    ///
    /// # Returns
    /// The frame to present, or `None` when nothing changed.
    pub fn render_window(&mut self, id: u32) -> Option<Frame<'_>> {
        if self.is_settings(id) {
            return self.render_settings();
        }
        let index = self.files.windows.iter().position(|w| w.id == id)?;
        let geometry = self.files.windows[index].geometry;
        if !geometry.configured || !self.files.windows[index].dirty {
            return None;
        }
        self.files.windows[index].dirty = false;
        let (w, h) = (
            (geometry.width * geometry.scale) as u32,
            (geometry.height * geometry.scale) as u32,
        );
        let mut pix = match self.files.windows[index].pix.take() {
            Some(pix) if pix.width() == w && pix.height() == h => pix,
            _ => Pixmap::new(w, h)?,
        };
        {
            let mut canvas = Canvas::new(pix.as_mut(), geometry.scale);
            ui::files::draw(self, id, &mut canvas);
        }
        let title = self.files.windows[index].title();
        if self.files.windows[index].shown_title != title {
            self.files.windows[index].shown_title = title.clone();
            self.requests.push(Request::SetTitle { id, title });
        }
        let whole = Rect::new(0, 0, geometry.width, geometry.height);
        self.files.windows[index].pix = Some(pix);
        Some(Frame {
            pixmap: self.files.windows[index].pix.as_ref()?,
            scale: geometry.scale,
            damage: vec![whole],
            input: vec![whole],
            visible: vec![whole],
            grab: false,
        })
    }

    /// Renders the settings window when it changed.
    ///
    /// # Returns
    /// The frame to present, or `None` when nothing changed.
    fn render_settings(&mut self) -> Option<Frame<'_>> {
        let geometry = self.settings.geometry;
        if !geometry.configured || !self.settings.dirty {
            return None;
        }
        self.settings.dirty = false;
        let (w, h) = (
            (geometry.width * geometry.scale) as u32,
            (geometry.height * geometry.scale) as u32,
        );
        let mut pix = match self.settings.pix.take() {
            Some(pix) if pix.width() == w && pix.height() == h => pix,
            _ => Pixmap::new(w, h)?,
        };
        {
            let mut canvas = Canvas::new(pix.as_mut(), geometry.scale);
            settings::draw(self, &mut canvas);
        }
        let whole = Rect::new(0, 0, geometry.width, geometry.height);
        self.settings.pix = Some(pix);
        Some(Frame {
            pixmap: self.settings.pix.as_ref()?,
            scale: geometry.scale,
            damage: vec![whole],
            input: vec![whole],
            visible: vec![whole],
            grab: false,
        })
    }
}
