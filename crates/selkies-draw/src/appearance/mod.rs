/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The session's appearance beyond the desktop's own surfaces: the toolkit settings
//! ([`toolkits`]) and the Settings portal ([`portal`]) that carry the desktop's light or dark
//! scheme and its accent to every application, and the rule by which that scheme follows the
//! browser a session is streamed to.
//!
//! One process keeps the appearance. Under the Selkies window manager it is the manager, in
//! both of its session modes, since the shell does not run while the session is the application
//! alone; the manager starts the shell with [`TOOLKITS`] off, so the two never write the same
//! stores. A shell under another window manager keeps it itself.
//!
//! The browser's scheme is followed through the desktop's own configuration rather than beside
//! it: while [`FOLLOW_BROWSER`] is on, which it is unless the configuration turns it off, the
//! light or dark scheme the primary display's browser asks for puts the Light or Dark scheme's
//! four colors into the user's file ([`browser_scheme_changes`]), the same keys the shell's
//! settings write. Every surface then follows the file as it follows a change of the user's: the
//! shell and the frames redraw, and the toolkits and the portal take the new decision. A scheme
//! or a color the user picks in the shell's settings turns following off, so the desktop keeps
//! what the user chose until the switch beside the schemes turns it back on.

pub mod portal;
pub mod toolkits;

use std::collections::HashMap;

use crate::render::parse_color;
use crate::style::{COLOR_KEYS, SCHEMES, Theme, parse_style, truthy};

/// The setting that lets the browser's light or dark scheme set the desktop's colors.
pub const FOLLOW_BROWSER: &str = "scheme_follows_browser";

/// The setting by which the shell keeps the appearance itself; the Selkies window manager starts
/// the shell with it off.
pub const TOOLKITS: &str = "toolkits";

/// An X display name without its screen number (`:20` for `:20.0`), so two names of one server
/// compare equal.
fn display_server(name: &str) -> &str {
    match name.rfind(':') {
        Some(colon) => match name[colon..].find('.') {
            Some(dot) => &name[..colon + dot],
            None => name,
        },
        None => name,
    }
}

/// Whether a process runs on the session's X display, by the `DISPLAY` its environment holds: a
/// desktop of the user's own sharing the home runs on another, and a setting written into its
/// configuration or a reload sent to it would change that desktop. A display that is `None` or
/// empty matches nothing.
pub fn on_session_display(pid: i32, display: Option<&str>) -> bool {
    let environ = std::fs::read(format!("/proc/{pid}/environ")).unwrap_or_default();
    shares_display(&environ, display)
}

/// [`on_session_display`] over a process's environment as `/proc/<pid>/environ` holds it.
fn shares_display(environ: &[u8], display: Option<&str>) -> bool {
    let Some(display) = display.filter(|name| !name.is_empty()).map(display_server) else {
        return false;
    };
    environ
        .split(|b| *b == 0)
        .filter_map(|pair| pair.strip_prefix(b"DISPLAY="))
        .any(|value| display_server(&String::from_utf8_lossy(value)) == display)
}

/// Whether the desktop's settings let the browser's scheme set its colors.
pub fn follows_browser(values: &HashMap<String, String>) -> bool {
    truthy(values, FOLLOW_BROWSER, true)
}

/// The changes to the desktop's settings that put the browser's scheme into effect: the Dark or
/// Light scheme's four colors, or `None` where the settings do not follow the browser or already
/// have those colors.
///
/// # Arguments
/// * `values` - The desktop's settings as [`crate::style::desktop_values`] reads them.
/// * `dark` - Whether the browser asks for the dark scheme.
pub fn browser_scheme_changes(
    values: &HashMap<String, String>,
    dark: bool,
) -> Option<Vec<(&'static str, String)>> {
    if !follows_browser(values) {
        return None;
    }
    let name = if dark { "Dark" } else { "Light" };
    let (_, colors) = SCHEMES.iter().find(|(scheme, _)| *scheme == name)?;
    let mut wanted = Theme::default();
    for (slot, color) in [
        &mut wanted.accent,
        &mut wanted.panel,
        &mut wanted.popup,
        &mut wanted.text,
    ]
    .into_iter()
    .zip(colors)
    {
        *slot = parse_color(color)?;
    }
    let (current, _) = parse_style(values);
    (current != wanted).then(|| {
        COLOR_KEYS
            .iter()
            .zip(colors)
            .map(|(key, color)| (*key, color.to_string()))
            .collect()
    })
}

/// The changes a user's own edit of the desktop's settings writes: one that changes a color
/// turns following the browser off as well, unless it names that setting itself, as a reset of
/// the page does.
///
/// # Arguments
/// * `changes` - The keys the edit sets and their values; an empty value removes the key.
pub fn user_changes<'a>(changes: &[(&'a str, String)]) -> Vec<(&'a str, String)> {
    let mut changes = changes.to_vec();
    if changes.iter().any(|(key, _)| COLOR_KEYS.contains(key))
        && !changes.iter().any(|(key, _)| *key == FOLLOW_BROWSER)
    {
        changes.push((FOLLOW_BROWSER, "false".to_string()));
    }
    changes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn a_process_shares_the_display_only_by_the_name_its_environment_holds() {
        let on_x = b"HOME=/h\0DISPLAY=:20.0\0";
        assert!(shares_display(on_x, Some(":20")));
        assert!(shares_display(on_x, Some(":20.0")));
        assert!(!shares_display(on_x, Some(":0")), "the user's own desktop");
        assert!(!shares_display(on_x, Some(":2")));
        assert!(!shares_display(on_x, None));
        assert!(!shares_display(on_x, Some("")));
        assert!(!shares_display(b"WAYLAND_DISPLAY=wayland-1\0", Some(":20")));
    }

    #[test]
    fn a_light_browser_puts_the_light_scheme_on_a_default_desktop() {
        let changes = browser_scheme_changes(&values(&[]), false).unwrap();
        assert_eq!(
            changes,
            vec![
                ("color_accent", "#3daee9".to_string()),
                ("color_panel", "#dee0e2".to_string()),
                ("color_popup", "#eff0f1".to_string()),
                ("color_text", "#232629".to_string()),
            ]
        );
    }

    #[test]
    fn a_desktop_already_on_the_browsers_scheme_is_left_as_it_is() {
        assert_eq!(browser_scheme_changes(&values(&[]), true), None);
        let light = values(&[
            ("color_accent", "#3daee9"),
            ("color_panel", "#dee0e2"),
            ("color_popup", "#eff0f1"),
            ("color_text", "#232629"),
        ]);
        assert_eq!(browser_scheme_changes(&light, false), None);
        assert!(browser_scheme_changes(&light, true).is_some());
    }

    #[test]
    fn a_users_own_color_turns_following_off_and_a_reset_does_not() {
        let picked = user_changes(&[("color_accent", "#b9671b".to_string())]);
        assert_eq!(picked.last(), Some(&(FOLLOW_BROWSER, "false".to_string())));
        let reset = user_changes(&[("color_accent", String::new()), (FOLLOW_BROWSER, String::new())]);
        assert_eq!(reset.len(), 2);
        let other = user_changes(&[("clock_format", "%H".to_string())]);
        assert_eq!(other, vec![("clock_format", "%H".to_string())]);
    }

    #[test]
    fn a_desktop_that_stopped_following_keeps_its_own_colors() {
        let own = values(&[(FOLLOW_BROWSER, "false"), ("color_accent", "#b9671b")]);
        assert_eq!(browser_scheme_changes(&own, false), None);
        assert_eq!(browser_scheme_changes(&own, true), None);
        let back = values(&[(FOLLOW_BROWSER, "true"), ("color_accent", "#b9671b")]);
        assert!(browser_scheme_changes(&back, true).is_some());
    }
}
