//! Settings: a small `key = value` file under the user's config directory,
//! each key overridable through a `SELKIES_DESKTOP_<KEY>` environment
//! variable, which is how Selkies containers are configured.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tiny_skia::Color;

use crate::render::{parse_color, rgb};

/// How a wallpaper image fills the screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WallpaperMode {
    /// Scaled to cover the screen, cropping the overflowing axis.
    Fill,
    /// Scaled to fit inside the screen, leaving bars of background color.
    Fit,
    /// Drawn at its native size in the middle of the screen.
    Center,
    /// Stretched to the screen, ignoring the aspect ratio.
    Stretch,
}

/// What is painted under the wallpaper image, or on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackgroundStyle {
    /// A flat color.
    Solid,
    /// A dithered gradient of the color.
    Gradient,
}

/// How one monitor's wallpaper is painted.
#[derive(Clone, Debug, PartialEq)]
pub struct Wallpaper {
    /// What is painted first.
    pub style: BackgroundStyle,
    /// The color of the background.
    pub color: Color,
    /// Image drawn over the background, if any.
    pub image: Option<PathBuf>,
    /// How the image fills the monitor.
    pub mode: WallpaperMode,
}

impl Default for Wallpaper {
    fn default() -> Wallpaper {
        Wallpaper {
            style: BackgroundStyle::Solid,
            color: rgb(0, 0, 0),
            image: None,
            mode: WallpaperMode::Fill,
        }
    }
}

/// The parts of a wallpaper a monitor overrides.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WallpaperOverride {
    /// Background style, when set for the monitor.
    pub style: Option<BackgroundStyle>,
    /// Background color, when set for the monitor.
    pub color: Option<Color>,
    /// Image, when set for the monitor; `Some(None)` for none at all.
    pub image: Option<Option<PathBuf>>,
    /// Image mode, when set for the monitor.
    pub mode: Option<WallpaperMode>,
}

impl WallpaperOverride {
    /// Whether nothing is overridden.
    pub fn is_empty(&self) -> bool {
        *self == WallpaperOverride::default()
    }
}

/// Which screen edge the bar sits on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelPosition {
    /// Along the bottom edge.
    Bottom,
    /// Along the top edge.
    Top,
}

pub use selkies_draw::style::{ButtonSide, Glyphs, Surface};

/// How the start menu lays its applications out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuLayout {
    /// The categories in a column and the chosen one's applications in rows.
    Columns,
    /// The categories in a column and the applications as a grid of icons.
    Grid,
    /// Every application in a list, with the favorites as tiles beside it.
    Tiles,
}

pub use selkies_draw::style::{DEFAULT_CORNERS, MAX_CORNERS, WindowStyle};

/// Everything the user can set.
#[derive(Clone, Debug)]
pub struct Config {
    /// Which edge the bar sits on.
    pub panel_position: PanelPosition,
    /// Height of the bar in logical pixels.
    pub panel_height: i32,
    /// Width of the bar as a percentage of the screen, centered.
    pub panel_width: i32,
    /// Opacity of the bar's background in percent.
    pub panel_opacity: i32,
    /// Whether the bar hides until the pointer touches its edge.
    pub panel_autohide: bool,
    /// Icon name or image path of the start button, or `None` for the Selkies logo.
    pub start_icon: Option<String>,
    /// Text shown on the start button beside its icon, if any.
    pub start_label: Option<String>,
    /// Whether the start button, quick launch and taskbar sit in the middle
    /// of the bar rather than at its left.
    pub panel_centered: bool,
    /// Space between the bar and the screen edges in logical pixels, which
    /// floats it.
    pub panel_margin: i32,
    /// How the start menu lays its applications out.
    pub menu_layout: MenuLayout,
    /// Language code of the interface, or `None` to follow the environment.
    pub language: Option<String>,
    /// The colors of the interface.
    pub theme: crate::ui::Theme,
    /// The look of window frames, the bar and the popups.
    pub window: WindowStyle,
    /// Desktop file IDs of the quick launch buttons beside the start button.
    pub launchers: Vec<String>,
    /// Whether the system tray is shown.
    pub show_tray: bool,
    /// Whether new file manager windows open in the list view.
    pub files_list: bool,
    /// What new file manager windows sort by.
    pub files_sort: String,
    /// Whether new file manager windows show hidden files.
    pub files_hidden: bool,
    /// The wallpaper of every monitor that has no override of its own.
    pub wallpaper: Wallpaper,
    /// Per-monitor wallpaper overrides, by monitor number starting at 1.
    pub monitors: HashMap<u32, WallpaperOverride>,
    /// strftime format of the panel clock.
    pub clock_format: String,
    /// Whether the clock is shown.
    pub show_clock: bool,
    /// Whether the volume control is shown when PulseAudio answers.
    pub show_volume: bool,
    /// Whether the show-desktop button is shown.
    pub show_desktop_button: bool,
    /// Desktop file IDs pinned at the top of the start menu.
    pub favorites: Vec<String>,
    /// Icon theme name, or `None` to detect one.
    pub icon_theme: Option<String>,
    /// Terminal command for entries that want one, or `None` to detect one.
    pub terminal: Option<String>,
    /// Whether the start menu remembers launched applications.
    pub track_recent: bool,
    /// Whether the built-in file manager is offered.
    pub files_manager: bool,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            panel_position: PanelPosition::Bottom,
            panel_height: crate::ui::PANEL_HEIGHT,
            panel_width: 100,
            panel_opacity: 95,
            panel_autohide: false,
            start_icon: None,
            start_label: None,
            panel_centered: false,
            panel_margin: 0,
            menu_layout: MenuLayout::Columns,
            language: None,
            theme: crate::ui::Theme::default(),
            window: WindowStyle::default(),
            launchers: Vec::new(),
            show_tray: true,
            files_list: false,
            files_sort: "name".to_string(),
            files_hidden: false,
            wallpaper: Wallpaper::default(),
            monitors: HashMap::new(),
            clock_format: "%H:%M".to_string(),
            show_clock: true,
            show_volume: true,
            show_desktop_button: true,
            favorites: Vec::new(),
            icon_theme: None,
            terminal: None,
            track_recent: true,
            files_manager: true,
        }
    }
}

pub use selkies_draw::dirs::{data_dirs, data_home, home};

/// The directory the config file and reload trigger live in.
pub use selkies_draw::style::desktop_config_dir as config_dir;

/// The directory for state the desktop keeps between sessions.
pub fn state_dir() -> PathBuf {
    selkies_draw::dirs::state_home().join("selkies-desktop")
}

/// Shortest bar the settings accept.
pub const MIN_PANEL_HEIGHT: i32 = 24;
/// Tallest bar the settings accept.
pub const MAX_PANEL_HEIGHT: i32 = 64;
/// Widest margin around the bar the settings accept.
pub const MAX_PANEL_MARGIN: i32 = 16;

/// The desktop file ID of the built-in file manager, the default favorite.
pub const FILES_ENTRY: &str = "selkies-desktop-files.desktop";

/// The config file path.
pub use selkies_draw::style::desktop_config_file as config_file;

/// Wallpaper files looked for when none is configured, in order.
fn default_wallpapers() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    for ext in ["png", "jpg", "jpeg"] {
        candidates.push(config_dir().join(format!("wallpaper.{ext}")));
    }
    for ext in ["png", "jpg", "jpeg"] {
        candidates.push(PathBuf::from(format!("/usr/share/selkies/wallpaper.{ext}")));
    }
    candidates.push(PathBuf::from(LOGO_PATH));
    candidates
}

impl Config {
    /// Reads the config file and the environment.
    pub fn load() -> Config {
        let values = selkies_draw::style::desktop_values();
        let mut config = Config::from_values(&values);
        if config.wallpaper.image.is_none() && !values.contains_key("wallpaper") {
            config.wallpaper.image = default_wallpapers().into_iter().find(|p| p.is_file());
            if !values.contains_key("wallpaper_mode")
                && config.wallpaper.image.as_deref() == Some(Path::new(LOGO_PATH))
            {
                config.wallpaper.mode = WallpaperMode::Center;
            }
        }
        if !values.contains_key("favorites") && config.files_manager {
            config.favorites = vec![FILES_ENTRY.to_string()];
        }
        config
    }

    /// Builds a config from parsed key and value pairs, defaults filling the
    /// gaps.  A wallpaper key with a `_N` suffix overrides that one key for
    /// monitor N.
    ///
    /// # Arguments
    /// * `values` - Lowercase keys and their raw values.
    pub fn from_values(values: &HashMap<String, String>) -> Config {
        let mut config = Config::default();
        if let Some(position) = values.get("panel_position") {
            config.panel_position = match position.trim().to_ascii_lowercase().as_str() {
                "top" => PanelPosition::Top,
                _ => PanelPosition::Bottom,
            };
        }
        if let Some(height) = values
            .get("panel_height")
            .and_then(|v| v.trim().parse::<i32>().ok())
        {
            config.panel_height = height.clamp(MIN_PANEL_HEIGHT, MAX_PANEL_HEIGHT);
        }
        if let Some(icon) = values
            .get("start_icon")
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
        {
            config.start_icon = Some(icon.to_string());
        }
        if let Some(label) = values
            .get("start_label")
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
        {
            config.start_label = Some(label.to_string());
        }
        if let Some(layout) = values.get("panel_layout").map(|v| v.trim().to_ascii_lowercase()) {
            config.panel_centered = layout == "centered" || layout == "center";
        }
        if let Some(margin) = values
            .get("panel_margin")
            .and_then(|v| v.trim().trim_end_matches("px").parse::<i32>().ok())
        {
            config.panel_margin = margin.clamp(0, MAX_PANEL_MARGIN);
        }
        if let Some(layout) = values.get("menu_layout").map(|v| v.trim().to_ascii_lowercase()) {
            config.menu_layout = match layout.as_str() {
                "grid" => MenuLayout::Grid,
                "tiles" => MenuLayout::Tiles,
                _ => MenuLayout::Columns,
            };
        }
        if let Some(width) = values
            .get("panel_width")
            .and_then(|v| v.trim().trim_end_matches('%').parse::<i32>().ok())
        {
            config.panel_width = width.clamp(20, 100);
        }
        if let Some(opacity) = values
            .get("panel_opacity")
            .and_then(|v| v.trim().trim_end_matches('%').parse::<i32>().ok())
        {
            config.panel_opacity = opacity.clamp(0, 100);
        }
        if let Some(list) = values.get("launchers") {
            config.launchers = id_list(list);
        }
        if let Some(sort) = values
            .get("files_sort")
            .map(|v| v.trim().to_ascii_lowercase())
            .filter(|v| ["name", "size", "modified", "type"].contains(&v.as_str()))
        {
            config.files_sort = sort;
        }
        if let Some(view) = values.get("files_view").map(|v| v.trim().to_ascii_lowercase()) {
            config.files_list = view == "list";
        }
        let truthy = |key: &str, current: bool| -> bool {
            match values.get(key).map(|v| v.trim().to_ascii_lowercase()) {
                Some(v) if ["true", "yes", "1", "on"].contains(&v.as_str()) => true,
                Some(v) if ["false", "no", "0", "off"].contains(&v.as_str()) => false,
                _ => current,
            }
        };
        let base = wallpaper_override(values, "");
        config.wallpaper = Wallpaper {
            style: base.style.unwrap_or(config.wallpaper.style),
            color: base.color.unwrap_or(config.wallpaper.color),
            image: base.image.unwrap_or(None),
            mode: base.mode.unwrap_or(config.wallpaper.mode),
        };
        for key in values.keys() {
            let Some((prefix, number)) = key.rsplit_once('_') else {
                continue;
            };
            if !WALLPAPER_KEYS.contains(&prefix) {
                continue;
            }
            let Ok(number) = number.parse::<u32>() else {
                continue;
            };
            if number == 0 || config.monitors.contains_key(&number) {
                continue;
            }
            let over = wallpaper_override(values, &format!("_{number}"));
            if !over.is_empty() {
                config.monitors.insert(number, over);
            }
        }
        if let Some(format) = values
            .get("clock_format")
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
        {
            config.clock_format = format.to_string();
        }
        config.show_clock = truthy("show_clock", config.show_clock);
        config.show_volume = truthy("show_volume", config.show_volume);
        config.show_desktop_button = truthy("show_desktop_button", config.show_desktop_button);
        config.show_tray = truthy("show_tray", config.show_tray);
        config.panel_autohide = truthy("panel_autohide", config.panel_autohide);
        (config.theme, config.window) = selkies_draw::style::parse_style(values);
        if let Some(language) = values.get("language").map(|v| v.trim()).filter(|v| !v.is_empty()) {
            config.language = Some(language.to_string());
        }
        config.files_hidden = truthy("files_hidden", config.files_hidden);
        if let Some(list) = values.get("favorites") {
            config.favorites = id_list(list);
        }
        if let Some(theme) = values
            .get("icon_theme")
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
        {
            config.icon_theme = Some(theme.to_string());
        }
        if let Some(terminal) = values.get("terminal").map(|v| v.trim()).filter(|v| !v.is_empty()) {
            config.terminal = Some(terminal.to_string());
        }
        config.track_recent = truthy("track_recent", config.track_recent);
        config.files_manager = truthy("files_manager", config.files_manager);
        config
    }

    /// The wallpaper a monitor is painted with: the shared one with the
    /// monitor's overrides applied.
    ///
    /// # Arguments
    /// * `monitor` - Monitor number, starting at 1.
    pub fn wallpaper_for(&self, monitor: u32) -> Wallpaper {
        let mut wallpaper = self.wallpaper.clone();
        if let Some(over) = self.monitors.get(&monitor) {
            if let Some(style) = over.style {
                wallpaper.style = style;
            }
            if let Some(color) = over.color {
                wallpaper.color = color;
            }
            if let Some(image) = &over.image {
                wallpaper.image = image.clone();
            }
            if let Some(mode) = over.mode {
                wallpaper.mode = mode;
            }
        }
        wallpaper
    }
}

/// The wallpaper keys, each taking an optional `_N` monitor suffix.
pub const WALLPAPER_KEYS: [&str; 4] = ["background_style", "background", "wallpaper", "wallpaper_mode"];

/// The keys of the bar's and the start menu's settings, which share a page.
pub const PANEL_KEYS: [&str; 18] = [
    "panel_position",
    "panel_height",
    "panel_width",
    "panel_opacity",
    "panel_autohide",
    "panel_layout",
    "panel_margin",
    "start_icon",
    "start_label",
    "menu_layout",
    "show_clock",
    "show_volume",
    "show_desktop_button",
    "show_tray",
    "clock_format",
    "launchers",
    "favorites",
    "track_recent",
];

/// The keys of the interface colors.
pub const COLOR_KEYS: [&str; 4] = ["color_accent", "color_panel", "color_popup", "color_text"];

/// The keys of the window frame settings.
pub const WINDOW_KEYS: [&str; 5] = [
    "theme_windows",
    "window_surface",
    "window_buttons",
    "window_glyphs",
    "window_corners",
];

/// The keys of the file manager's settings.
pub const FILES_KEYS: [&str; 3] = ["files_view", "files_sort", "files_hidden"];

/// The Selkies logo, the wallpaper when no other image is configured.
pub const LOGO_PATH: &str = "/usr/share/selkies/www/icon.png";

/// Parses a list of desktop file IDs, the `.desktop` suffix added where missing.
///
/// # Arguments
/// * `list` - IDs separated by commas, semicolons or spaces.
pub fn id_list(list: &str) -> Vec<String> {
    list.split([',', ';', ' '])
        .map(|s| s.trim())
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("none"))
        .map(|s| {
            if s.ends_with(".desktop") {
                s.to_string()
            } else {
                format!("{s}.desktop")
            }
        })
        .collect()
}

/// Reads the wallpaper keys with a suffix into an override.
fn wallpaper_override(values: &HashMap<String, String>, suffix: &str) -> WallpaperOverride {
    let get = |key: &str| {
        values
            .get(&format!("{key}{suffix}"))
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
    };
    let mut over = WallpaperOverride::default();
    if let Some(style) = get("background_style") {
        over.style = Some(match style.to_ascii_lowercase().as_str() {
            "solid" | "color" | "flat" => BackgroundStyle::Solid,
            _ => BackgroundStyle::Gradient,
        });
    }
    if let Some(color) = get("background").and_then(parse_color) {
        over.color = Some(color);
    }
    if let Some(path) = get("wallpaper") {
        if path.eq_ignore_ascii_case("none") {
            over.image = Some(None);
        } else {
            let expanded = if let Some(rest) = path.strip_prefix("~/") {
                home().join(rest)
            } else {
                PathBuf::from(path)
            };
            over.image = Some(expanded.is_file().then_some(expanded));
        }
    }
    if let Some(mode) = get("wallpaper_mode") {
        over.mode = Some(match mode.to_ascii_lowercase().as_str() {
            "fit" => WallpaperMode::Fit,
            "center" => WallpaperMode::Center,
            "stretch" => WallpaperMode::Stretch,
            _ => WallpaperMode::Fill,
        });
    }
    over
}

/// Writes settings into the config file, replacing the lines of the keys
/// given and keeping every other line, comments included.
///
/// # Arguments
/// * `changes` - Keys and their new values; an empty value removes the key.
///
/// # Errors
/// The file could not be written.
pub fn save(changes: &[(&str, String)]) -> std::io::Result<()> {
    let path = config_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut lines: Vec<String> = Vec::new();
    let mut written: Vec<&str> = Vec::new();
    for line in existing.lines() {
        let trimmed = line.trim();
        let key = trimmed
            .split_once('=')
            .filter(|_| !trimmed.starts_with('#'))
            .map(|(k, _)| k.trim().to_ascii_lowercase().replace('-', "_"));
        match key.and_then(|k| changes.iter().find(|(name, _)| *name == k)) {
            Some((name, value)) => {
                if !written.contains(name) {
                    if !value.is_empty() {
                        lines.push(format!("{name} = {}", quote(value)));
                    }
                    written.push(name);
                }
            }
            None => lines.push(line.to_string()),
        }
    }
    for (name, value) in changes {
        if !written.contains(name) && !value.is_empty() {
            lines.push(format!("{name} = {}", quote(value)));
        }
    }
    let mut text = lines.join("\n");
    text.push('\n');
    std::fs::write(&path, text)
}

/// Quotes a value when it has leading or trailing spaces or a comment mark.
fn quote(value: &str) -> String {
    if value != value.trim() || value.contains('#') {
        format!("\"{}\"", value.replace('"', ""))
    } else {
        value.to_string()
    }
}

#[cfg(test)]
use selkies_draw::style::parse_values as parse;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lines() {
        let values = parse("# comment\nclock_format = \"%a %H:%M\"\nShow-Clock=false\n\nbroken line\n");
        assert_eq!(values.get("clock_format").map(String::as_str), Some("%a %H:%M"));
        assert_eq!(values.get("show_clock").map(String::as_str), Some("false"));
        assert_eq!(values.len(), 2);
    }

    #[test]
    fn saves_keeping_other_lines() {
        let dir = std::env::temp_dir().join(format!("selkies-desktop-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &dir);
        }
        std::fs::create_dir_all(config_dir()).unwrap();
        std::fs::write(config_file(), "# mine\nclock_format = %H\nfavorites = foot\n").unwrap();
        save(&[
            ("clock_format", "%H:%M".to_string()),
            ("panel_position", "top".to_string()),
        ])
        .unwrap();
        let text = std::fs::read_to_string(config_file()).unwrap();
        assert_eq!(
            text,
            "# mine\nclock_format = %H:%M\nfavorites = foot\npanel_position = top\n"
        );
        save(&[("favorites", String::new())]).unwrap();
        let values = parse(&std::fs::read_to_string(config_file()).unwrap());
        assert!(!values.contains_key("favorites"));
        assert_eq!(values.get("panel_position").map(String::as_str), Some("top"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn builds_config() {
        let mut values = HashMap::new();
        values.insert("show_clock".to_string(), "no".to_string());
        values.insert("favorites".to_string(), "foot, firefox.desktop".to_string());
        values.insert("background".to_string(), "#102030".to_string());
        values.insert("wallpaper_mode".to_string(), "Center".to_string());
        values.insert("background_2".to_string(), "#ff0000".to_string());
        values.insert("wallpaper_2".to_string(), "none".to_string());
        let config = Config::from_values(&values);
        assert!(!config.show_clock);
        assert!(config.show_volume);
        assert_eq!(
            config.favorites,
            vec!["foot.desktop".to_string(), "firefox.desktop".to_string()]
        );
        assert_eq!(config.wallpaper.color.to_color_u8().green(), 0x20);
        assert_eq!(config.wallpaper.mode, WallpaperMode::Center);
        assert_eq!(config.wallpaper_for(1).color.to_color_u8().green(), 0x20);
        let second = config.wallpaper_for(2);
        assert_eq!(second.color.to_color_u8().red(), 0xff);
        assert_eq!(second.mode, WallpaperMode::Center);
        assert_eq!(second.image, None);
        assert!(config.monitors.contains_key(&2));
    }

    #[test]
    fn builds_window_style() {
        let mut values = HashMap::new();
        values.insert("window_surface".to_string(), "Glossy".to_string());
        values.insert("window_buttons".to_string(), "left".to_string());
        values.insert("window_glyphs".to_string(), "circles".to_string());
        values.insert("window_corners".to_string(), "40".to_string());
        values.insert("theme_windows".to_string(), "no".to_string());
        values.insert("panel_layout".to_string(), "Centered".to_string());
        values.insert("panel_margin".to_string(), "8px".to_string());
        values.insert("menu_layout".to_string(), "grid".to_string());
        values.insert("start_label".to_string(), " Start ".to_string());
        let config = Config::from_values(&values);
        assert!(config.panel_centered);
        assert_eq!(config.panel_margin, 8);
        assert_eq!(config.menu_layout, MenuLayout::Grid);
        assert_eq!(config.start_label.as_deref(), Some("Start"));
        assert_eq!(config.window.surface, Surface::Glossy);
        assert_eq!(config.window.buttons, ButtonSide::Left);
        assert_eq!(config.window.glyphs, Glyphs::Circles);
        assert_eq!(config.window.corners, MAX_CORNERS);
        assert!(!config.window.decorate);
        assert_eq!(Config::default().window, WindowStyle::default());
    }

    #[test]
    fn falls_back_on_unknown_words_and_clamps() {
        let mut values = HashMap::new();
        values.insert("window_surface".to_string(), "shiny".to_string());
        values.insert("window_buttons".to_string(), "middle".to_string());
        values.insert("window_glyphs".to_string(), "stars".to_string());
        values.insert("window_corners".to_string(), "-3".to_string());
        values.insert("menu_layout".to_string(), "spiral".to_string());
        values.insert("panel_layout".to_string(), "center".to_string());
        values.insert("panel_margin".to_string(), "99".to_string());
        values.insert("start_label".to_string(), "   ".to_string());
        let config = Config::from_values(&values);
        assert_eq!(config.window.surface, Surface::Flat);
        assert_eq!(config.window.buttons, ButtonSide::Right);
        assert_eq!(config.window.glyphs, Glyphs::Lines);
        assert_eq!(config.window.corners, 0);
        assert_eq!(config.menu_layout, MenuLayout::Columns);
        assert!(config.panel_centered);
        assert_eq!(config.panel_margin, MAX_PANEL_MARGIN);
        assert_eq!(config.start_label, None);
        let saved = parse("panel_layout = centered\nmenu_layout = tiles\nstart_label = \"Start \"\n");
        let config = Config::from_values(&saved);
        assert!(config.panel_centered);
        assert_eq!(config.menu_layout, MenuLayout::Tiles);
        assert_eq!(config.start_label.as_deref(), Some("Start"));
    }
}
