//! The desktop described for a reader that cannot see it, answered on the control socket.
//!
//! The shell draws every control itself, so no toolkit describes it on the accessibility bus.
//! Each part of the interface already lays its controls out in rectangles, which it draws and
//! hit-tests with, so it describes them from the same layout (a `describe` beside each `hit`):
//! what a reader is told is where a click lands. The answer is `pelorus`'s
//! [`Description`]: one [`Surface`] for each window of the shell's own, found by the reader
//! among the window manager's windows by its title and its number, and one for each bar and
//! the popup open over it, placed at the origin the backend anchored the surface at
//! ([`crate::backend::Backend::surface_origin`]). Rectangles are the surface's logical
//! coordinates, the window manager's layout units on either backend.
//!
//! A control another surface hides is left out where the shell knows it is hidden (scrolled
//! out of its list, or under the settings window's color picker), and so is a bar, with its
//! popup, while a fullscreen window covers it; what other windows cover is the reader's to
//! judge from the window manager's stacking. Names are in the desktop's language, as a person
//! reading the screen would see them.

pub use pelorus::Element;
use pelorus::shell::{Description, Surface};

use crate::desktop::{Desktop, Geometry, Popup};
use crate::event::SurfaceId;
use crate::geom::Rect;
use crate::i18n::{tr, trf};
use crate::ui;

/// How many keys a bar and its popups take: a bar's key is its slot (the panel 0, the other
/// displays' bars from 1) times this, plus [`BAR`], and a popup's adds its own offset.
const BAR_KEYS: u32 = 16;
/// The bar's offset within its slot's keys.
const BAR: u32 = 1;

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
    let window = |window: u32, tree: Element| Surface {
        key: 0,
        window,
        name: tree.name.clone(),
        origin: None,
        tree,
    };
    if let Some(id) = d.settings.id
        && let Some(tree) = ui::settings::describe(d)
    {
        surfaces.push(window(id, tree));
    }
    if let Some(id) = d.dialog.id
        && let Some(tree) = ui::dialog::describe(d)
    {
        surfaces.push(window(id, tree));
    }
    for properties in &d.properties {
        if let Some(tree) = ui::properties::describe(d, properties.id) {
            surfaces.push(window(properties.id, tree));
        }
    }
    let ids: Vec<u32> = d.files.windows.iter().map(|w| w.id).collect();
    for id in ids {
        if let Some(tree) = ui::files::describe(d, id) {
            surfaces.push(window(id, tree));
        }
    }
    surfaces.extend(describe_bar(d, 0, &origin));
    for (slot, id) in (1..).zip(d.bar_ids()) {
        if let Some(bar) = d.with_bar(id, |d| describe_bar(d, slot, &origin)) {
            surfaces.extend(bar);
        }
    }
    Description { surfaces }
}

/// The bar standing in for the panel and the popup open on it, keyed by the bar's slot; nothing
/// while a fullscreen window covers the bar, since the compositor draws the window over it.
fn describe_bar(
    d: &mut Desktop,
    slot: u32,
    origin: &impl Fn(SurfaceId) -> Option<(i32, i32)>,
) -> Vec<Surface> {
    let surface = d.current_bar_surface();
    let Some(at) = d.panel.configured.then(|| origin(surface)).flatten() else {
        return Vec::new();
    };
    if d.current_bar_covered() {
        return Vec::new();
    }
    let base = slot * BAR_KEYS;
    let named = |name: &str| {
        if slot == 0 {
            name.to_string()
        } else {
            trf(
                "{name} on display {n}",
                &[("name", name), ("n", &(slot + 1).to_string())],
            )
        }
    };
    let mut surfaces = vec![Surface {
        key: base + BAR,
        window: 0,
        name: named(tr("Panel")),
        origin: Some(at),
        tree: ui::panel::describe(d),
    }];
    if d.popup_on_current_bar()
        && let Some((offset, name, tree)) = describe_popup(d)
    {
        surfaces.push(Surface {
            key: base + offset,
            window: 0,
            name: named(name),
            origin: Some(at),
            tree,
        });
    }
    surfaces
}

/// The open popup: its key's offset within its bar's, its name, and its controls.
fn describe_popup(d: &mut Desktop) -> Option<(u32, &'static str, Element)> {
    Some(match d.popup {
        Popup::None => return None,
        Popup::Menu => (2, tr("Start menu"), ui::menu::describe(d)?),
        Popup::Launcher => (3, tr("Quick launcher"), ui::launcher::describe(d)?),
        Popup::Volume => (4, tr("Volume"), ui::popups::describe_volume(d)?),
        Popup::Calendar => (5, tr("Calendar"), ui::popups::describe_calendar(d)?),
        Popup::Notifications => (6, tr("Notifications"), ui::notifications::describe_center(d)?),
        Popup::TaskMenu(index) => (7, tr("Window menu"), ui::popups::describe_task_menu(d, index)?),
        Popup::PanelMenu(x, _) => (8, tr("Menu"), ui::settings::describe_panel_menu(d, x)),
        Popup::DesktopMenu(x, y) => (9, tr("Menu"), ui::desktop_menu::describe(d, (x, y))),
        Popup::TrayMenu => (10, tr("Menu"), ui::tray_menu::describe(d)?),
    })
}
