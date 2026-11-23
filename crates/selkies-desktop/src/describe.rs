//! The desktop described for a reader that cannot see it, answered on the control socket.
//!
//! The shell draws every control itself, so no toolkit describes it on the accessibility bus.
//! Each part of the interface already lays its controls out in rectangles, which it draws and
//! hit-tests with, so it describes them from the same layout (a `describe` beside each `hit`):
//! what a reader is told is where a click lands. The answer is `pelorus`'s
//! [`Description`]: one [`Surface`] for each window of the shell's own, found by the reader
//! among the window manager's windows by its title, and one for the bar and for each popup
//! open over it, placed at the origin the backend anchored the surface at
//! ([`crate::backend::Backend::surface_origin`]). Rectangles are the surface's logical
//! coordinates, the window manager's layout units on either backend.
//!
//! A control another surface hides is left out where the shell knows it is hidden (scrolled
//! out of its list, or under the settings window's color picker); what other windows cover is
//! the reader's to judge from the window manager's stacking.

pub use pelorus::Element;
use pelorus::shell::{Description, Surface};

use crate::desktop::{Desktop, Geometry, Popup};
use crate::event::SurfaceId;
use crate::geom::Rect;
use crate::ui;

/// The key of the panel's surface; a display's bar counts from [`BAR_KEYS`] times one more
/// than its display's number, its popups from there as the panel's from this.
const PANEL: u32 = 1;
/// The key of the start menu's surface, after its bar's.
const MENU: u32 = 1;
/// The key of the quick launcher's surface, after its bar's.
const LAUNCHER: u32 = 2;
/// How many keys a bar and its popups take.
const BAR_KEYS: u32 = 16;

/// A rectangle as the description writes it.
pub fn rect(r: Rect) -> pelorus::Rect {
    pelorus::Rect {
        x: r.x,
        y: r.y,
        w: r.w,
        h: r.h,
    }
}

/// A control of a role, named, over a rectangle.
pub fn node(role: &str, name: impl Into<String>, r: Rect) -> Element {
    pelorus::shell::control(role, name, rect(r))
}

/// A control holding others.
pub fn holding(role: &str, name: impl Into<String>, r: Rect, children: Vec<Element>) -> Element {
    pelorus::shell::group(role, name, rect(r), children)
}

/// A window of the shell's own, its whole surface holding its controls.
pub fn frame(title: &str, geometry: Geometry, children: Vec<Element>) -> Element {
    holding(
        "frame",
        title,
        Rect::new(0, 0, geometry.width, geometry.height),
        children,
    )
}

/// Whether a rectangle's center, where a click on it aims, lies inside an area.
pub fn shows_in(r: Rect, area: Rect) -> bool {
    area.contains(f64::from(r.x + r.w / 2), f64::from(r.y + r.h / 2))
}

/// Everything the desktop shows.
///
/// # Arguments
/// * `d` - The desktop.
/// * `origin` - Where the backend anchored a surface, in the window manager's layout.
pub fn describe(d: &mut Desktop, origin: impl Fn(SurfaceId) -> Option<(i32, i32)>) -> Description {
    let mut surfaces = Vec::new();
    let window = |title: String, tree: Element| Surface {
        key: 0,
        name: title,
        origin: None,
        tree,
    };
    if let Some(tree) = ui::settings::describe(d) {
        surfaces.push(window(tree.name.clone(), tree));
    }
    if let Some(tree) = ui::dialog::describe(d) {
        surfaces.push(window(tree.name.clone(), tree));
    }
    let ids: Vec<u32> = d.files.windows.iter().map(|w| w.id).collect();
    for id in ids {
        if let Some(tree) = ui::files::describe(d, id) {
            surfaces.push(window(tree.name.clone(), tree));
        }
    }
    surfaces.extend(describe_bar(d, &origin));
    for id in d.bar_ids() {
        if let Some(bar) = d.with_bar(id, |d| describe_bar(d, &origin)) {
            surfaces.extend(bar);
        }
    }
    Description { surfaces }
}

/// The bar standing in for the panel and the popup open on it.
fn describe_bar(d: &mut Desktop, origin: &impl Fn(SurfaceId) -> Option<(i32, i32)>) -> Vec<Surface> {
    let surface = d.current_bar_surface();
    let Some(at) = d.panel.configured.then(|| origin(surface)).flatten() else {
        return Vec::new();
    };
    let (key, suffix) = match surface {
        SurfaceId::Bar(id) => ((id + 1) * BAR_KEYS, format!(" on display {id}")),
        _ => (PANEL, String::new()),
    };
    let mut surfaces = vec![Surface {
        key,
        name: format!("Panel{suffix}"),
        origin: Some(at),
        tree: ui::panel::describe(d),
    }];
    if d.popup_on_current_bar() {
        let popup = match d.popup {
            Popup::Menu => ui::menu::describe(d).map(|tree| (MENU, "Start menu", tree)),
            Popup::Launcher => ui::launcher::describe(d).map(|tree| (LAUNCHER, "Quick launcher", tree)),
            _ => None,
        };
        if let Some((offset, name, tree)) = popup {
            surfaces.push(Surface {
                key: key + offset,
                name: format!("{name}{suffix}"),
                origin: Some(at),
                tree,
            });
        }
    }
    surfaces
}
