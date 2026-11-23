/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! What no application describes on the accessibility bus: the window manager's open menu and
//! the desktop shell's own surfaces, both drawn by Selkies itself.
//!
//! Neither goes through the bus, since both already hold every control's rectangle. The window
//! manager's menu levels come with the desktop view ([`DesktopView::menus`]). The shell
//! (`selkies-desktop`) answers `describe` on its control socket with `pelorus::shell`'s
//! description, read here per request, and the socket's peer credentials name its process, so
//! a window of that process is told apart from an application's window of the same title.
//!
//! Whatever listens on that socket is a process of the session user, as the shell is, so its
//! answer is read as untrusted as the text on the screen: bounded in size and time, and only
//! ever turned into lines a model reads and points a click goes to.
//!
//! A shell window is described where the window manager put it. The menu and the surfaces the
//! window manager does not manage (the bar and the popups over it) have no window of their own,
//! so they are listed as windows numbered from [`MENU_WINDOW`] and [`SHELL_SURFACES`], far
//! above any number a window manager gives, and a look at one reads it without activating
//! anything, since there is no window to raise and raising another would close a popup.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use ::pelorus::shell::{self, Description};
use ::pelorus::{Element, Rect as PxRect};
use pixelflux::wm::{DesktopView, MenuEntryKind, MenuView, Rect, WindowView};

use super::{px_rect, scale_at};

/// The number the window manager's open menu is listed under.
pub const MENU_WINDOW: u32 = 0xFFFE_0000;

/// The first number the shell's surfaces are listed under, each its key past it.
pub const SHELL_SURFACES: u32 = 0xFFFF_0000;

/// How long the shell may take to answer.
const SHELL_TIMEOUT: Duration = Duration::from_millis(800);

/// The most bytes of the shell's answer read; a longer one is refused.
const SHELL_ANSWER_LIMIT: u64 = 4 << 20;

/// Whether a number names something listed here rather than a window.
pub fn is_surface(id: u32) -> bool {
    id >= MENU_WINDOW
}

/// The desktop shell's description of itself and its process.
pub struct Shell {
    pub pid: u32,
    pub description: Description,
}

/// Ask the running shell to describe itself; `None` where no shell answers. Blocks for up to
/// [`SHELL_TIMEOUT`] each way.
pub fn read_shell() -> Option<Shell> {
    let env = crate::wm::launch_env()?;
    let path = selkies_draw::style::desktop_socket_path(|name| env.get(name).cloned());
    let mut stream = UnixStream::connect(path).ok()?;
    let pid = nix::sys::socket::getsockopt(&stream, nix::sys::socket::sockopt::PeerCredentials)
        .ok()
        .and_then(|credentials| u32::try_from(credentials.pid()).ok())?;
    stream.set_read_timeout(Some(SHELL_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(SHELL_TIMEOUT)).ok()?;
    stream
        .write_all(format!("{}\n", shell::DESCRIBE).as_bytes())
        .ok()?;
    stream.shutdown(std::net::Shutdown::Write).ok()?;
    let mut answer = String::new();
    stream
        .take(SHELL_ANSWER_LIMIT + 1)
        .read_to_string(&mut answer)
        .ok()?;
    if answer.len() as u64 > SHELL_ANSWER_LIMIT {
        return None;
    }
    let description = serde_json::from_str(answer.trim()).ok()?;
    Some(Shell { pid, description })
}

/// Something listed as a window that is not one, described and placed in pixels.
pub struct Surface {
    pub id: u32,
    pub title: String,
    pub tree: Element,
    /// Where its controls are; a point outside is off-screen.
    pub clip: PxRect,
}

/// The surfaces listed as windows, top first: the window manager's menu, which lies over
/// everything, then the shell's popups over its bars.
pub fn surfaces(view: &DesktopView, shell: Option<&Shell>) -> Vec<Surface> {
    let mut out = Vec::new();
    if let Some(menu) = menu_surface(view) {
        out.push(menu);
    }
    if let Some(shell) = shell {
        for surface in shell.description.surfaces.iter().rev() {
            let Some(origin) = surface.origin else {
                continue;
            };
            let scale = scale_at(&view.outputs, origin.0, origin.1);
            let tree = shell::placed(&surface.tree, origin, scale);
            let clip = tree.bounds.unwrap_or_default();
            out.push(Surface {
                id: SHELL_SURFACES + surface.key,
                title: surface.name.clone(),
                tree,
                clip,
            });
        }
    }
    out
}

/// A shell window's controls, placed where the window manager put the window; `None` for a
/// window of any other process, or one the shell did not describe.
pub fn shell_window(window: &WindowView, scale: f64, shell: &Shell) -> Option<Element> {
    if window.info.pid != Some(shell.pid) {
        return None;
    }
    let surface = shell
        .description
        .surfaces
        .iter()
        .find(|s| s.origin.is_none() && s.name == window.info.title)?;
    Some(shell::placed(&surface.tree, window.origin, scale))
}

/// The window manager's open menu, every level's rows under one surface.
fn menu_surface(view: &DesktopView) -> Option<Surface> {
    let first = view.menus.first()?;
    let title = match first.id.as_str() {
        "root-menu" => "Desktop menu",
        "client-menu" => "Window menu",
        _ => "Menu",
    };
    let levels: Vec<Element> = view
        .menus
        .iter()
        .map(|level| menu_level(level, scale_at(&view.outputs, level.rect.x, level.rect.y)))
        .collect();
    let clip = view
        .menus
        .iter()
        .map(|level| level.rect)
        .reduce(union)
        .unwrap_or(first.rect);
    let scale = scale_at(&view.outputs, clip.x, clip.y);
    let clip = px_rect(clip, scale);
    Some(Surface {
        id: MENU_WINDOW,
        title: title.to_string(),
        tree: shell::group("panel", "", clip, levels),
        clip,
    })
}

/// One level of the window manager's menu in pixels at `scale`.
fn menu_level(level: &MenuView, scale: f64) -> Element {
    let rows = level
        .entries
        .iter()
        .map(|entry| {
            let rect = px_rect(entry.rect, scale);
            match entry.kind {
                MenuEntryKind::Separator if entry.label.is_empty() => {
                    shell::control("separator", "", rect)
                }
                MenuEntryKind::Separator => shell::control("label", entry.label.clone(), rect),
                MenuEntryKind::Item | MenuEntryKind::Submenu => {
                    let mut item = shell::control("menu item", entry.label.clone(), rect);
                    item.states.enabled = entry.enabled;
                    item.states.checked = entry.checked;
                    if entry.kind == MenuEntryKind::Submenu {
                        item.states.expandable = true;
                        item.states.expanded = entry.open;
                    }
                    item
                }
            }
        })
        .collect();
    shell::group("panel", "", px_rect(level.rect, scale), rows)
}

/// The smallest rectangle holding both.
fn union(a: Rect, b: Rect) -> Rect {
    let (x, y) = (a.x.min(b.x), a.y.min(b.y));
    Rect::new(
        x,
        y,
        (a.x + a.w).max(b.x + b.w) - x,
        (a.y + a.h).max(b.y + b.h) - y,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pixelflux::wm::{MenuEntryView, OutputInfo};

    fn view(menus: Vec<MenuView>) -> DesktopView {
        DesktopView {
            outputs: vec![OutputInfo {
                id: 0,
                rect: Rect::new(0, 0, 960, 540),
                workarea: Rect::new(0, 0, 960, 540),
                scale: 2.0,
                primary: true,
            }],
            menus,
            ..DesktopView::default()
        }
    }

    #[test]
    fn the_window_managers_open_menu_is_listed_in_pixels_with_its_rows() {
        let level = MenuView {
            id: "root-menu".to_string(),
            rect: Rect::new(100, 100, 200, 60),
            entries: vec![
                MenuEntryView {
                    label: "Terminal".to_string(),
                    rect: Rect::new(100, 104, 200, 26),
                    enabled: true,
                    ..MenuEntryView::default()
                },
                MenuEntryView {
                    label: "Applications".to_string(),
                    kind: MenuEntryKind::Submenu,
                    rect: Rect::new(100, 130, 200, 26),
                    enabled: true,
                    open: true,
                    ..MenuEntryView::default()
                },
            ],
        };
        let listed = surfaces(&view(vec![level]), None);
        assert_eq!(listed.len(), 1);
        let menu = &listed[0];
        assert!(is_surface(menu.id));
        assert_eq!(menu.title, "Desktop menu");
        let rows = &menu.tree.children[0].children;
        assert_eq!(
            rows[0].bounds.map(|r| (r.x, r.y, r.w, r.h)),
            Some((200, 208, 400, 52))
        );
        assert!(rows[1].states.expanded);
    }

    #[test]
    fn nothing_is_listed_without_a_menu_or_a_shell() {
        assert!(surfaces(&view(Vec::new()), None).is_empty());
    }
}
