/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The look the desktop and its window manager share: the four colors a user sets, the window
//! style the frames and the panel follow, the color arithmetic that derives everything else, and
//! the reader of the desktop's `key = value` configuration both read it from.
//!
//! The shell keeps the configuration under `~/.config/selkies-desktop/config` with
//! `SELKIES_DESKTOP_<KEY>` environment variables over it. The window manager reads the same keys
//! through [`desktop_values`] and [`parse_style`], so a theme changed in the shell's settings
//! reaches the frames without a second format or a message between the two: the shell writes the
//! file, the window manager sees it change.

use std::collections::HashMap;
use std::path::{Component, PathBuf};

use tiny_skia::Color;

use crate::render::{parse_color, rgb, rgba};

/// The colors the user can set: the four the rest derive from. The defaults are the desktop's
/// Dark preset: a sky-blue accent on neutral dark grays.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Theme {
    /// Markers, selections, switches, and the active choice.
    pub accent: Color,
    /// The bar, and the active window's title bar.
    pub panel: Color,
    /// Popups, menus, and windows.
    pub popup: Color,
    /// Text.
    pub text: Color,
}

impl Default for Theme {
    fn default() -> Theme {
        Theme {
            accent: rgb(0x3d, 0xae, 0xe9),
            panel: rgb(0x29, 0x2c, 0x30),
            popup: rgb(0x20, 0x23, 0x26),
            text: rgb(0xfc, 0xfc, 0xfc),
        }
    }
}

/// How the title bars of windows, the bar, and the popups are shaded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Surface {
    /// One flat color.
    #[default]
    Flat,
    /// A soft gradient, lighter at the top.
    Gradient,
    /// A gloss: a lighter upper half over a darker lower half.
    Glossy,
}

/// Which side of a title bar the window buttons sit on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ButtonSide {
    /// At the right, the Linux and Windows way.
    #[default]
    Right,
    /// At the left, the macOS way.
    Left,
}

/// The shapes of the window buttons.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Glyphs {
    /// Thin line glyphs in square buttons.
    #[default]
    Lines,
    /// Colored circles.
    Circles,
    /// Thin line glyphs in wide flat buttons, the title at the left.
    Wide,
}

/// The look of window frames, which the bar and the popups follow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowStyle {
    /// Whether the window manager's frames and menus are themed at all.
    pub decorate: bool,
    /// How title bars are shaded.
    pub surface: Surface,
    /// Which side the buttons sit on.
    pub buttons: ButtonSide,
    /// The shapes of the buttons.
    pub glyphs: Glyphs,
    /// Corner radius of windows and popups in logical pixels.
    pub corners: i32,
}

impl Default for WindowStyle {
    fn default() -> WindowStyle {
        WindowStyle {
            decorate: true,
            surface: Surface::Flat,
            buttons: ButtonSide::Right,
            glyphs: Glyphs::Lines,
            corners: DEFAULT_CORNERS,
        }
    }
}

/// Corner radius of windows and popups unless set otherwise.
pub const DEFAULT_CORNERS: i32 = 8;
/// Largest corner radius the settings accept.
pub const MAX_CORNERS: i32 = 12;

/// The colors of the circle buttons on an active window: close, iconify, and maximize.
pub const CIRCLES: [&str; 3] = ["#ff5f57", "#febc2e", "#28c840"];

/// The desktop's configuration directory.
pub fn desktop_config_dir() -> PathBuf {
    crate::dirs::config_home().join("selkies-desktop")
}

/// The desktop's configuration file.
pub fn desktop_config_file() -> PathBuf {
    desktop_config_dir().join("config")
}

/// The socket the running desktop shell takes commands on: under the runtime directory, named
/// after the display it runs on so one shell per session is reachable, read from the
/// environment a variable lookup gives, which the window manager fills from the environment it
/// starts the shell with.
///
/// # Arguments
/// * `var` - Looks a variable up: `XDG_RUNTIME_DIR`, `WAYLAND_DISPLAY`, `DISPLAY`.
pub fn desktop_socket_path(var: impl Fn(&str) -> Option<String>) -> PathBuf {
    let set = |name: &str| var(name).filter(|v| !v.is_empty());
    let dir = set("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let display = set("WAYLAND_DISPLAY")
        .or_else(|| set("DISPLAY"))
        .unwrap_or_else(|| "default".to_string());
    let safe: String = display
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    dir.join(format!("selkies-desktop-{safe}.sock"))
}

/// The desktop's settings as lowercase keys and raw values: an image's defaults in
/// `selkies-desktop/config` under the system configuration directories (`/etc/xdg`, the first of
/// `XDG_CONFIG_DIRS` winning), the user's configuration file over them, and the
/// `SELKIES_DESKTOP_<KEY>` environment over both, which the shell's settings window writes to the
/// user's file and so cannot change.
pub fn desktop_values() -> HashMap<String, String> {
    layered_values(
        &crate::dirs::config_dirs(),
        &desktop_config_file(),
        std::env::vars(),
    )
}

/// The desktop's settings from the system directories (the first winning), the user's file
/// over them, and the `SELKIES_DESKTOP_<KEY>` variables over both, as [`desktop_values`] reads
/// them.
fn layered_values(
    system: &[std::path::PathBuf],
    user: &std::path::Path,
    env: impl Iterator<Item = (String, String)>,
) -> HashMap<String, String> {
    let mut values = HashMap::new();
    for dir in system.iter().rev() {
        let path = dir.join("selkies-desktop/config");
        if !(path.is_absolute() && !path.components().any(|c| c == Component::ParentDir)) {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path) {
            values.extend(parse_values(&text));
        }
    }
    let text = 'user: {
        if !(user.is_absolute() && !user.components().any(|c| c == Component::ParentDir)) {
            break 'user String::new();
        }
        std::fs::read_to_string(user).unwrap_or_default()
    };
    values.extend(parse_values(&text));
    for (key, value) in env {
        if let Some(name) = key.strip_prefix("SELKIES_DESKTOP_")
            && !name.is_empty()
        {
            values.insert(name.to_ascii_lowercase(), value);
        }
    }
    values
}

/// Parses `key = value` lines, keys lowercased with dashes as underscores, quotes around a value
/// dropped, `#` lines and blank lines skipped.
pub fn parse_values(text: &str) -> HashMap<String, String> {
    let mut values = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase().replace('-', "_");
        let mut value = value.trim();
        if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            value = &value[1..value.len() - 1];
        }
        values.insert(key, value.to_string());
    }
    values
}

/// A boolean setting's value, `current` where the key is missing or says neither.
pub fn truthy(values: &HashMap<String, String>, key: &str, current: bool) -> bool {
    match values.get(key).map(|v| v.trim().to_ascii_lowercase()) {
        Some(v) if ["true", "yes", "1", "on"].contains(&v.as_str()) => true,
        Some(v) if ["false", "no", "0", "off"].contains(&v.as_str()) => false,
        _ => current,
    }
}

/// The keys of the four colors a [`Theme`] is, in its order: accent, bar, menus, and text.
pub const COLOR_KEYS: [&str; 4] = ["color_accent", "color_panel", "color_popup", "color_text"];

/// The color schemes the desktop offers: the four colors of each, as [`COLOR_KEYS`] orders them.
/// Dark, the default, and Light share a sky-blue accent on neutral grays, the bar the shade of a
/// window's header and the menus a window's own; Ember is a copper accent on warm charcoal, dark
/// enough to take white text; Mint is a green accent on pale sage, a light desktop of a warmer
/// cast than Light; and Graphite is flat neutral grays with a light gray accent, a dark desktop
/// without a hue.
pub const SCHEMES: [(&str, [&str; 4]); 5] = [
    ("Dark", ["#3daee9", "#292c30", "#202326", "#fcfcfc"]),
    ("Light", ["#3daee9", "#dee0e2", "#eff0f1", "#232629"]),
    ("Ember", ["#b9671b", "#1c1917", "#272220", "#f4ede4"]),
    ("Mint", ["#13866a", "#d8e9e2", "#f2f8f5", "#14302a"]),
    ("Graphite", ["#cccccc", "#2c2c2c", "#303030", "#dfdfdf"]),
];

/// Writes settings into a `key = value` file, replacing the lines of the keys given and keeping
/// every other line, comments included. The shell and the window manager both write the file
/// and each reads it as it changes, so a write holds an exclusive lock on a `.lock` file beside
/// it from its read to its rename, and the file is replaced in that one rename: two writers
/// never drop each other's keys, and a reader never sees half a file. A link is followed, so the
/// file it names is the one replaced.
///
/// # Arguments
/// * `path` - The file, created with its directory when missing.
/// * `changes` - Keys and their new values; an empty value removes the key.
///
/// # Errors
/// The file could not be written.
pub fn save_values(path: &std::path::Path, changes: &[(&str, String)]) -> std::io::Result<()> {
    let path = &std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut lock_path = path.as_os_str().to_owned();
    lock_path.push(".lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(PathBuf::from(lock_path))?;
    lock.lock()?;
    let existing = std::fs::read_to_string(path).unwrap_or_default();
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
    let mut staged = path.as_os_str().to_owned();
    staged.push(format!(".{}.tmp", std::process::id()));
    let staged = PathBuf::from(staged);
    std::fs::write(&staged, text)?;
    std::fs::rename(&staged, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&staged);
    })
}

/// Quotes a value when it has leading or trailing spaces or a comment mark.
fn quote(value: &str) -> String {
    if value != value.trim() || value.contains('#') {
        format!("\"{}\"", value.replace('"', ""))
    } else {
        value.to_string()
    }
}

/// The theme and the window style the desktop's settings describe, defaults filling the gaps.
pub fn parse_style(values: &HashMap<String, String>) -> (Theme, WindowStyle) {
    let defaults = Theme::default();
    let color = |key: &str, current: Color| values.get(key).and_then(|v| parse_color(v)).unwrap_or(current);
    let theme = Theme {
        accent: color("color_accent", defaults.accent),
        panel: color("color_panel", defaults.panel),
        popup: color("color_popup", defaults.popup),
        text: color("color_text", defaults.text),
    };
    let mut style = WindowStyle::default();
    style.decorate = truthy(values, "theme_windows", style.decorate);
    if let Some(surface) = values
        .get("window_surface")
        .map(|v| v.trim().to_ascii_lowercase())
    {
        style.surface = match surface.as_str() {
            "gradient" => Surface::Gradient,
            "glossy" => Surface::Glossy,
            _ => Surface::Flat,
        };
    }
    if let Some(side) = values
        .get("window_buttons")
        .map(|v| v.trim().to_ascii_lowercase())
    {
        style.buttons = if side == "left" {
            ButtonSide::Left
        } else {
            ButtonSide::Right
        };
    }
    if let Some(glyphs) = values.get("window_glyphs").map(|v| v.trim().to_ascii_lowercase()) {
        style.glyphs = match glyphs.as_str() {
            "circles" => Glyphs::Circles,
            "wide" => Glyphs::Wide,
            _ => Glyphs::Lines,
        };
    }
    if let Some(corners) = values
        .get("window_corners")
        .and_then(|v| v.trim().trim_end_matches("px").parse::<i32>().ok())
    {
        style.corners = corners.clamp(0, MAX_CORNERS);
    }
    (theme, style)
}

/// A color with another alpha.
pub fn with_alpha(color: Color, alpha: u8) -> Color {
    let c = color.to_color_u8();
    rgba(c.red(), c.green(), c.blue(), alpha)
}

/// A color moved towards black or white by `by` steps a channel, negative to darken.
pub fn shifted(color: Color, by: i32) -> Color {
    let c = color.to_color_u8();
    let step = |v: u8| (v as i32 + by).clamp(0, 255) as u8;
    rgba(step(c.red()), step(c.green()), step(c.blue()), c.alpha())
}

/// A color mixed towards another by `amount` (0 for none to 1 for all), the alpha of the first
/// kept.
pub fn blend(color: Color, towards: Color, amount: f32) -> Color {
    let amount = amount.clamp(0.0, 1.0);
    let mix = |a: f32, b: f32| a + (b - a) * amount;
    Color::from_rgba(
        mix(color.red(), towards.red()),
        mix(color.green(), towards.green()),
        mix(color.blue(), towards.blue()),
        color.alpha(),
    )
    .unwrap_or(color)
}

/// The perceived lightness of a color, 0 for black to 1 for white.
pub fn luminance(color: Color) -> f32 {
    0.2126 * color.red() + 0.7152 * color.green() + 0.0722 * color.blue()
}

/// Black or white, whichever reads on a color. The threshold sits above a sky-blue accent (a
/// luminance of 0.60), which takes white text, and below a gold accent, which takes black.
pub fn on_color(color: Color) -> Color {
    if luminance(color) > 0.62 {
        rgb(0x10, 0x10, 0x10)
    } else {
        Color::WHITE
    }
}

/// The color of a glossy surface at a vertical position `v` (0 at the top, 1 at the bottom): a
/// lighter band down to `split`, fading a little, then the base darkening slightly to the bottom.
pub fn gloss(base: Color, v: f32, split: f32) -> Color {
    if v < split {
        shifted(base, (22.0 - 10.0 * v / split).round() as i32)
    } else {
        shifted(
            base,
            (-6.0 * (v - split) / (1.0 - split).max(0.01)).round() as i32,
        )
    }
}

/// The color of a surface at vertical position `v` (0 at the top, 1 at the bottom) in a surface
/// style: the base flat, lightened towards the top for the gradient, and a lighter upper half over
/// a darker lower half for the gloss.
pub fn surface_shade_in(surface: Surface, base: Color, v: f32) -> Color {
    match surface {
        Surface::Flat => base,
        Surface::Gradient => shifted(base, (14.0 * (1.0 - v)).round() as i32),
        Surface::Glossy => gloss(base, v, 0.5),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two writers saving at once each keep the other's keys, since each holds the lock from its
    /// read to its rename.
    #[test]
    fn writers_saving_at_once_keep_each_others_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        let writers: Vec<_> = ["first", "second"]
            .into_iter()
            .map(|key| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for round in 0..200 {
                        save_values(&path, &[(key, round.to_string())]).unwrap();
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }
        let values = parse_values(&std::fs::read_to_string(&path).unwrap());
        assert_eq!(values.get("first").map(String::as_str), Some("199"));
        assert_eq!(values.get("second").map(String::as_str), Some("199"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// An image's defaults under the system directories give way to the user's file, and both
    /// to the environment; of two system directories the first wins.
    #[test]
    fn image_defaults_give_way_to_the_user_and_the_environment() {
        let root = std::env::temp_dir().join(format!("selkies-draw-layers-{}", std::process::id()));
        let (first, second) = (root.join("first"), root.join("second"));
        for (dir, text) in [
            (&first, "terminals = false\nfavorites = a.desktop\n"),
            (
                &second,
                "terminals = true\nrun_command = false\nsettings = false\n",
            ),
        ] {
            std::fs::create_dir_all(dir.join("selkies-desktop")).unwrap();
            std::fs::write(dir.join("selkies-desktop/config"), text).unwrap();
        }
        let user = root.join("user-config");
        std::fs::write(&user, "favorites = b.desktop\nsettings = true\n").unwrap();
        let env = vec![("SELKIES_DESKTOP_SETTINGS".to_string(), "false".to_string())];
        let values = layered_values(&[first, second], &user, env.into_iter());
        assert_eq!(values["terminals"], "false");
        assert_eq!(values["run_command"], "false");
        assert_eq!(values["favorites"], "b.desktop");
        assert_eq!(values["settings"], "false");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The window style keys parse as the shell's settings page writes them, out-of-range values
    /// falling back to the defaults.
    #[test]
    fn the_window_style_parses_from_the_desktop_keys() {
        let values = parse_values(
            "window_surface = Glossy\nwindow-buttons = \"left\"\nwindow_glyphs = circles\nwindow_corners = 40px\ncolor_accent = #ff0000\n",
        );
        let (theme, style) = parse_style(&values);
        assert_eq!(style.surface, Surface::Glossy);
        assert_eq!(style.buttons, ButtonSide::Left);
        assert_eq!(style.glyphs, Glyphs::Circles);
        assert_eq!(style.corners, MAX_CORNERS);
        assert_eq!(theme.accent.to_color_u8().red(), 0xff);
        assert_eq!(theme.panel, Theme::default().panel);
        let (_, odd) = parse_style(&parse_values("window_surface = shiny\nwindow_corners = -3\n"));
        assert_eq!(odd.surface, Surface::Flat);
        assert_eq!(odd.corners, 0);
    }

    /// A blend keeps the first color's alpha and lands between the two.
    #[test]
    fn a_blend_lands_between_its_colors() {
        let mixed = blend(rgba(0, 0, 0, 0x80), rgb(0xff, 0xff, 0xff), 0.5).to_color_u8();
        assert_eq!(mixed.alpha(), 0x80);
        assert!((0x7e..=0x81).contains(&mixed.red()));
        assert_eq!(on_color(rgb(0xff, 0xff, 0xff)), rgb(0x10, 0x10, 0x10));
        assert_eq!(on_color(rgb(0, 0, 0)), Color::WHITE);
    }

    /// The defaults are the Dark scheme, whose sky-blue accent takes white text.
    #[test]
    fn the_default_theme_is_the_dark_scheme() {
        let theme = Theme::default();
        assert_eq!(theme.accent, rgb(0x3d, 0xae, 0xe9));
        assert_eq!(theme.panel, rgb(0x29, 0x2c, 0x30));
        assert_eq!(theme.popup, rgb(0x20, 0x23, 0x26));
        assert_eq!(theme.text, rgb(0xfc, 0xfc, 0xfc));
        assert_eq!(on_color(theme.accent), Color::WHITE);
    }
}
