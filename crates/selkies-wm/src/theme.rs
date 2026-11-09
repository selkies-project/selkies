/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The look of the frames and menus: the desktop shell's own theme, read from its configuration
//! and derived into a palette the way the shell derived the theme files it wrote for labwc and
//! openbox, so a frame looks the same whichever manager draws it. There is no theme file engine:
//! the shell's four colors and its window style are the whole theme, and an `rc.xml` naming an
//! openbox theme is logged and otherwise left to the icon theme it may name.
//!
//! The shell writes its settings to one file as a user changes them, so [`ThemeWatch`] reads
//! that file's modification time once a second and reloads the palette when it moved. The
//! shell's own window style for frames it does not draw (whether frames are themed at all, the
//! button side, the glyphs, the corner radius) is read from the same keys.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use selkies_draw::style::{self, Surface, Theme, WindowStyle};
use selkies_draw::tiny_skia::Color;

/// The colors of the frames and menus, derived from the theme.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    /// Title bar of the active window, the base of its shading.
    pub title_active: Color,
    /// Title bar of an inactive window.
    pub title_inactive: Color,
    /// Border of the active window.
    pub border_active: Color,
    /// Border of an inactive window.
    pub border_inactive: Color,
    /// Title text of the active window.
    pub label_active: Color,
    /// Title text of an inactive window.
    pub label_inactive: Color,
    /// Button glyphs of the active window, when they are not circles.
    pub button_active: Color,
    /// Button glyphs of an inactive window.
    pub button_inactive: Color,
    /// Background of menus.
    pub menu_bg: Color,
    /// Text of menus.
    pub menu_text: Color,
    /// Background of the highlighted menu item.
    pub menu_active_bg: Color,
    /// Text of the highlighted menu item.
    pub menu_active_text: Color,
    /// Border of menus.
    pub menu_border: Color,
    /// Separators in menus.
    pub menu_separator: Color,
    /// Text of disabled menu items.
    pub menu_disabled: Color,
    /// Background of menu headers.
    pub menu_title_bg: Color,
    /// The accent, for on-screen displays and hovered buttons.
    pub accent: Color,
}

impl Palette {
    /// The palette the shell's colors derive into.
    pub fn new(theme: Theme) -> Palette {
        let title_inactive = style::blend(theme.panel, theme.popup, 0.5);
        Palette {
            title_active: theme.panel,
            title_inactive,
            border_active: style::blend(theme.panel, theme.text, 0.18),
            border_inactive: style::blend(title_inactive, theme.text, 0.12),
            label_active: theme.text,
            label_inactive: style::blend(title_inactive, theme.text, 0.55),
            button_active: theme.text,
            button_inactive: style::blend(title_inactive, theme.text, 0.4),
            menu_bg: theme.popup,
            menu_text: theme.text,
            menu_active_bg: theme.accent,
            menu_active_text: style::on_color(theme.accent),
            menu_border: style::blend(theme.popup, theme.text, 0.14),
            menu_separator: style::blend(theme.popup, theme.text, 0.11),
            menu_disabled: style::blend(theme.popup, theme.text, 0.4),
            menu_title_bg: theme.panel,
            accent: theme.accent,
        }
    }

    /// The color of a title bar at vertical position `v` (0 at the top, 1 at the bottom) in a
    /// surface style.
    pub fn title_shade(&self, surface: Surface, active: bool, v: f32) -> Color {
        let base = if active {
            self.title_active
        } else {
            self.title_inactive
        };
        style::surface_shade_in(surface, base, v)
    }

    /// The color of a circle button: 0 for close, 1 for iconify, 2 for maximize.
    pub fn circle(which: usize) -> Color {
        selkies_draw::render::parse_color(style::CIRCLES[which.min(2)]).unwrap_or(Color::WHITE)
    }
}

/// Everything the frames and menus are drawn from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Look {
    pub theme: Theme,
    pub style: WindowStyle,
    pub palette: Palette,
}

impl Look {
    /// The look the desktop's settings describe.
    pub fn from_values(values: &HashMap<String, String>) -> Look {
        let (theme, style) = style::parse_style(values);
        Look {
            theme,
            style,
            palette: Palette::new(theme),
        }
    }
}

impl Default for Look {
    fn default() -> Look {
        Look::from_values(&HashMap::new())
    }
}

/// How often the configuration file is looked at.
const POLL: Duration = Duration::from_secs(1);

/// The shell's configuration file, watched by its modification time.
pub struct ThemeWatch {
    path: PathBuf,
    seen: Option<SystemTime>,
    checked: Instant,
    pub look: Look,
    /// The icon theme the desktop names, for the frames' and menus' icons.
    pub icon_theme: Option<String>,
}

impl ThemeWatch {
    /// Read the shell's settings now and remember the file's time.
    pub fn new() -> ThemeWatch {
        let path = style::desktop_config_file();
        let seen = modified(&path);
        let values = style::desktop_values();
        ThemeWatch {
            path,
            seen,
            checked: Instant::now(),
            look: Look::from_values(&values),
            icon_theme: icon_theme(&values),
        }
    }

    /// Whether the file changed since the last look; the look is reloaded when it did. Looks
    /// at the file no more than once a [`POLL`].
    pub fn poll(&mut self, now: Instant) -> bool {
        if now.duration_since(self.checked) < POLL {
            return false;
        }
        self.checked = now;
        let modified = modified(&self.path);
        if modified == self.seen {
            return false;
        }
        self.seen = modified;
        let values = style::desktop_values();
        let look = Look::from_values(&values);
        let icon_theme = icon_theme(&values);
        let changed = look != self.look || icon_theme != self.icon_theme;
        self.look = look;
        self.icon_theme = icon_theme;
        changed
    }

    /// When the watch next wants to be polled.
    pub fn next_poll(&self) -> Instant {
        self.checked + POLL
    }
}

impl Default for ThemeWatch {
    fn default() -> ThemeWatch {
        ThemeWatch::new()
    }
}

fn modified(path: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn icon_theme(values: &HashMap<String, String>) -> Option<String> {
    values
        .get("icon_theme")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The palette derives the inactive title between the panel and the popup colors and picks
    /// a menu highlight text that reads on the accent.
    #[test]
    fn the_palette_derives_from_the_four_colors() {
        let palette = Palette::new(Theme::default());
        assert_eq!(palette.title_active, Theme::default().panel);
        assert_ne!(palette.title_inactive, palette.title_active);
        assert_eq!(palette.menu_active_text, Color::WHITE);
        assert_eq!(
            palette.title_shade(Surface::Flat, true, 0.5),
            palette.title_active
        );
        assert_ne!(
            palette.title_shade(Surface::Gradient, true, 0.0),
            palette.title_shade(Surface::Gradient, true, 1.0)
        );
    }
}
