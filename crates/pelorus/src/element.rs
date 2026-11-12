/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! One control of a window and where it is on the screen.
//!
//! A toolkit reports a control's rectangle twice over the accessibility bus, in "screen" and in
//! "window" coordinates, and no two agree on what either counts from, so [`Placement::locate`]
//! picks per toolkit and window protocol, in the pixels input is injected in:
//!
//! | window | toolkit | rule |
//! | --- | --- | --- |
//! | Wayland | GTK 3, Chromium, Gecko | the surface's corner plus the window coordinate |
//! | Wayland | GTK 4 | the client area's corner plus the window coordinate |
//! | X11 | GTK 4 | the window's corner plus the window coordinate |
//! | X11 | every other | the screen coordinate |
//!
//! A Wayland client cannot know where its window is, so its "screen" coordinate is its window
//! one (GTK 4 answers zero on either protocol). The surface's corner lies outside the client
//! area by the shadow the client draws, which differs per application and with its state, so it
//! is read from the compositor with the tree. GTK 4 counts from the window it shows rather
//! than from its surface, and sums each ancestor's offset to do it, which leaves some subtrees
//! a dozen pixels off: a point is a control's center for that reason, never its corner. Qt 5
//! answers window coordinates that are no coordinates at all, which is why an X11 window takes
//! the screen ones wherever a toolkit gives them.

use serde::Serialize;

/// An integer rectangle in screen pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    /// The point a click on the rectangle aims at.
    pub fn center(&self) -> (i32, i32) {
        (self.x + self.w / 2, self.y + self.h / 2)
    }

    /// Whether the point lies inside.
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.w && y < self.y + self.h
    }
}

/// The protocol a window arrived through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    Wayland,
    X11,
}

/// The toolkit behind an application, as far as its coordinates differ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Toolkit {
    Gtk4,
    /// Chromium and what is built on it (Chrome, Electron).
    Chromium,
    /// Every toolkit that follows the common rule: GTK 3, Qt, Gecko, and the unknown.
    Other,
}

impl Toolkit {
    /// The toolkit an application names itself as over the bus, with its version.
    pub fn named(name: &str, version: &str) -> Toolkit {
        if name.eq_ignore_ascii_case("gtk") && version.starts_with('4') {
            Toolkit::Gtk4
        } else if name.eq_ignore_ascii_case("chromium") {
            Toolkit::Chromium
        } else {
            Toolkit::Other
        }
    }
}

/// Where the window manager put a window, in screen pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    pub protocol: Protocol,
    /// The client area.
    pub rect: Rect,
    /// The corner the client's own coordinates count from: its surface's on Wayland, its
    /// window's on X11.
    pub origin: (i32, i32),
    /// Pixels per unit of the client's coordinates: the output's scale on Wayland, one on X11,
    /// whose clients count in pixels.
    pub scale: f64,
}

impl Placement {
    /// A control's rectangle on the screen from the two the toolkit reported, by the module's
    /// table.
    pub fn locate(&self, toolkit: Toolkit, screen: Rect, window: Rect) -> Rect {
        let base = match (self.protocol, toolkit) {
            (Protocol::X11, Toolkit::Gtk4) => self.origin,
            (Protocol::X11, _) => return screen,
            (Protocol::Wayland, Toolkit::Gtk4) => (self.rect.x, self.rect.y),
            (Protocol::Wayland, _) => self.origin,
        };
        let scaled = |v: i32| (f64::from(v) * self.scale).round() as i32;
        Rect {
            x: base.0 + scaled(window.x),
            y: base.1 + scaled(window.y),
            w: scaled(window.w),
            h: scaled(window.h),
        }
    }
}

/// The states of a control a reader acts on, from the accessibility state set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct States {
    pub showing: bool,
    pub enabled: bool,
    pub focused: bool,
    pub checked: bool,
    pub selected: bool,
    pub pressed: bool,
    pub expanded: bool,
    pub expandable: bool,
    pub editable: bool,
}

/// One control: what it is, what it says, and where a click reaches it.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Element {
    /// The accessibility role's name, as the toolkit spells it (`button`, `menu item`).
    pub role: String,
    pub name: String,
    /// The text a text control holds, or a value control's value.
    pub text: Option<String>,
    /// Where a link goes.
    pub target: Option<String>,
    pub states: States,
    /// The control's rectangle on the screen; `None` where the toolkit gave it no extent.
    pub bounds: Option<Rect>,
    pub children: Vec<Element>,
}

impl Element {
    /// How many controls the tree holds, this one included.
    pub fn count(&self) -> usize {
        1 + self.children.iter().map(Element::count).sum::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Rect = Rect {
        x: 1257,
        y: 20,
        w: 28,
        h: 41,
    };
    const WINDOW: Rect = Rect {
        x: 282,
        y: 0,
        w: 28,
        h: 41,
    };

    fn placement(protocol: Protocol, scale: f64) -> Placement {
        Placement {
            protocol,
            rect: Rect {
                x: 52,
                y: 0,
                w: 972,
                h: 706,
            },
            origin: (36, -10),
            scale,
        }
    }

    #[test]
    fn a_wayland_client_counts_from_its_surface_corner() {
        let at = placement(Protocol::Wayland, 1.0).locate(Toolkit::Chromium, WINDOW, WINDOW);
        assert_eq!((at.x, at.y, at.w, at.h), (318, -10, 28, 41));
    }

    #[test]
    fn gtk4_counts_from_the_window_it_shows() {
        let at = placement(Protocol::Wayland, 1.0).locate(Toolkit::Gtk4, Rect::default(), WINDOW);
        assert_eq!((at.x, at.y), (334, 0));
        let at = placement(Protocol::X11, 1.0).locate(Toolkit::Gtk4, Rect::default(), WINDOW);
        assert_eq!((at.x, at.y), (318, -10));
    }

    #[test]
    fn an_x11_client_is_taken_at_its_screen_coordinates() {
        let at = placement(Protocol::X11, 1.0).locate(Toolkit::Other, SCREEN, WINDOW);
        assert_eq!(at, SCREEN);
    }

    #[test]
    fn a_scaled_output_multiplies_the_clients_units() {
        let at = placement(Protocol::Wayland, 2.0).locate(Toolkit::Other, WINDOW, WINDOW);
        assert_eq!((at.x, at.y, at.w, at.h), (600, -10, 56, 82));
    }

    #[test]
    fn the_toolkit_is_read_from_its_name_and_version() {
        assert_eq!(Toolkit::named("GTK", "4.22.4"), Toolkit::Gtk4);
        assert_eq!(Toolkit::named("gtk", "3.24.52"), Toolkit::Other);
        assert_eq!(Toolkit::named("Chromium", "1.0"), Toolkit::Chromium);
        assert_eq!(Toolkit::named("Qt", "6.10.2"), Toolkit::Other);
    }
}
