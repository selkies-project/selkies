/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The Selkies desktop shell's own surfaces, as the shell describes them.
//!
//! The shell draws its bar, its menus, and its windows itself, so no toolkit describes them on
//! the accessibility bus. It does not need one: every control's rectangle is in the layout it
//! draws and hit-tests with, so it answers the [`DESCRIBE`] line on its control socket with a
//! [`Description`], one line of JSON, read where a reader needs it rather than kept current on
//! a bus. Each [`Surface`] is a tree of [`Element`]s whose bounds are in the surface's own
//! logical coordinates, which are the window manager's logical layout units on either backend.
//!
//! The reader places each surface, since the shell cannot know where all of them are: a window
//! of the shell's own where the window manager put the window of that title, and a surface the
//! window manager does not manage (the bar and the popups over it, a layer surface on Wayland)
//! at the [`Surface::origin`] the shell gives it, since it anchored the surface itself.
//! [`placed`] then turns the tree into screen pixels.

use serde::{Deserialize, Serialize};

use crate::element::{Element, Rect, States};

/// The line that asks the shell for its [`Description`].
pub const DESCRIBE: &str = "describe";

/// Every [`Surface::key`] is under this; a reader drops a surface whose key is not.
pub const KEY_LIMIT: u32 = 1 << 16;

/// Everything the shell shows.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Description {
    pub surfaces: Vec<Surface>,
}

/// One surface of the shell and its controls.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Surface {
    /// A number that names a surface the window manager does not manage the same way across
    /// reads while it shows (the bar, the start menu), under [`KEY_LIMIT`]; 0 for a window.
    pub key: u32,
    /// The shell's own number for a window, which counts up as windows open, so windows of one
    /// title are told apart by the order they opened in; 0 for any other surface.
    pub window: u32,
    /// A window's title, by which a reader finds it among the window manager's windows, or the
    /// name of a surface the window manager does not manage.
    pub name: String,
    /// Where the surface's top-left corner lies in the window manager's logical layout; `None`
    /// for a window, which the window manager placed.
    pub origin: Option<(i32, i32)>,
    /// The surface's controls, their bounds relative to its top-left corner.
    pub tree: Element,
}

/// A control of a role, named, over a rectangle.
pub fn control(role: &str, name: impl Into<String>, rect: Rect) -> Element {
    Element {
        role: role.to_string(),
        name: name.into(),
        states: States {
            showing: true,
            enabled: true,
            ..States::default()
        },
        bounds: Some(rect),
        ..Element::default()
    }
}

/// A control holding others, named, over a rectangle.
pub fn group(role: &str, name: impl Into<String>, rect: Rect, children: Vec<Element>) -> Element {
    Element {
        children,
        ..control(role, name, rect)
    }
}

/// A tree whose bounds are relative to a surface whose corner lies at `origin` in logical
/// layout units, moved into screen pixels at `scale`.
pub fn placed(mut tree: Element, origin: (i32, i32), scale: f64) -> Element {
    place(&mut tree, origin, scale);
    tree
}

/// Moves every rectangle of a tree into screen pixels, in place.
fn place(tree: &mut Element, origin: (i32, i32), scale: f64) {
    let px = |v: i32| (f64::from(v) * scale).round() as i32;
    tree.bounds = tree.bounds.map(|r| {
        let (x, y) = (px(origin.0 + r.x), px(origin.1 + r.y));
        Rect {
            x,
            y,
            w: px(origin.0 + r.x + r.w) - x,
            h: px(origin.1 + r.y + r.h) - y,
        }
    });
    for child in &mut tree.children {
        place(child, origin, scale);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_surface_is_placed_at_its_origin_and_scaled_to_pixels() {
        let tree = group(
            "frame",
            "Settings",
            Rect {
                x: 0,
                y: 0,
                w: 400,
                h: 300,
            },
            vec![control(
                "button",
                "Reset",
                Rect {
                    x: 10,
                    y: 20,
                    w: 30,
                    h: 15,
                },
            )],
        );
        let placed = placed(tree, (100, 50), 1.5);
        assert_eq!(
            placed.children[0].bounds,
            Some(Rect {
                x: 165,
                y: 105,
                w: 45,
                h: 23,
            })
        );
        assert_eq!(placed.bounds.map(|r| (r.x, r.y)), Some((150, 75)));
    }

    #[test]
    fn a_description_reads_back_as_it_was_written() {
        let description = Description {
            surfaces: vec![Surface {
                key: 2,
                window: 0,
                name: "Start menu".to_string(),
                origin: Some((0, 1040)),
                tree: control(
                    "menu item",
                    "Terminal",
                    Rect {
                        x: 4,
                        y: 4,
                        w: 200,
                        h: 44,
                    },
                ),
            }],
        };
        let line = serde_json::to_string(&description).unwrap();
        assert_eq!(
            serde_json::from_str::<Description>(&line).unwrap(),
            description
        );
    }
}
