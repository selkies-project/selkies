/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A window's controls as the text a model reads.
//!
//! One line a control, indented under what holds it: its role, its name in quotes, the text
//! it holds after `=`, where a link goes after `->`, what state it is in where that is not
//! the ordinary one, and how to click it. Only a control something can be done to can be
//! clicked, so a model is not invited to click a label.
//!
//! How a control is clicked is written one of two ways ([`Marks`]). For a caller that places
//! the pointer itself the line ends in `@x,y`, the screen point. For a model the line starts
//! with a reference, `[e41]`, and the points stay with whoever wrote the outline: a small
//! model asked to copy a dozen coordinates out of a list pairs some with the wrong control,
//! and one token per control is something it does not get wrong. The letter keeps a reference
//! from reading as content: numbered bare, a calculator's "7" key can come out as `[7]` and
//! its "1" key as `[13]`, and a model asked to press 1 then presses 7.
//!
//! The accessibility tree is far wordier than what a person sees: most of its objects are
//! boxes that hold other boxes. A container with no name and no text is left out and what it
//! holds moves up in its place; separators and scroll bars are dropped; a label that only
//! repeats the name of the control around it is dropped. A point outside the window's client
//! area is marked `off-screen` (the control is scrolled out of view), and one another window
//! lies over is marked `covered`, which the caller decides from the window manager's stacking
//! order: a click at either would land somewhere else. The outline stops at a line budget and
//! says how many controls it left out, so a reader knows it saw a part.

use std::fmt::Write as _;

use crate::element::{Element, Rect};

/// The roles a person acts on with a pointer.
const INTERACTIVE: &[&str] = &[
    "button",
    "push button menu",
    "toggle button",
    "check box",
    "radio button",
    "switch",
    "menu",
    "menu item",
    "check menu item",
    "radio menu item",
    "entry",
    "text",
    "password text",
    "editbar",
    "combo box",
    "link",
    "page tab",
    "slider",
    "spin button",
    "list item",
    "tree item",
    "table cell",
    "icon",
    "terminal",
    "tool tip",
];

/// The roles that are never worth a line.
const NOISE: &[&str] = &["separator", "scroll bar"];

/// The roles that are text and nothing else.
const LABELS: &[&str] = &["label", "static", "heading", "paragraph", "caption"];

/// The longest a name or a text is written.
const TEXT_SHOWN: usize = 160;

/// How a clickable control is marked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Marks {
    /// `@x,y` after the line: the screen point.
    Points,
    /// `[en]` before it: a reference, its number counted up from `first`, whose point is the
    /// matching entry of [`Outline::points`].
    Refs { first: u32 },
}

/// An outline and the points of the controls it marked, in the order it marked them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outline {
    pub text: String,
    pub points: Vec<(i32, i32)>,
}

/// What decides how an outline is cut and marked.
pub struct Options<'a> {
    pub marks: Marks,
    /// The window's client area: a point outside it is off-screen.
    pub clip: Rect,
    /// Whether another window lies over a screen point.
    pub covered: &'a dyn Fn(i32, i32) -> bool,
    /// The most lines written.
    pub lines: usize,
}

/// Whether a control is one a pointer acts on.
fn interactive(element: &Element) -> bool {
    INTERACTIVE.contains(&element.role.as_str())
}

/// Whether a control says nothing itself and only holds others.
fn folds(element: &Element) -> bool {
    element.name.trim().is_empty() && element.text.is_none() && !interactive(element)
}

/// A string cut to what an outline shows, on one line.
fn shown(text: &str) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > TEXT_SHOWN {
        let cut: String = flat.chars().take(TEXT_SHOWN).collect();
        format!("{cut}…")
    } else {
        flat
    }
}

struct Writer<'a> {
    options: &'a Options<'a>,
    points: Vec<(i32, i32)>,
    out: String,
    lines: usize,
    left_out: usize,
}

impl Writer<'_> {
    fn line(&mut self, element: &Element, depth: usize) {
        if self.lines >= self.options.lines {
            self.left_out += 1;
            return;
        }
        self.lines += 1;
        let _ = write!(self.out, "{:indent$}", "", indent = depth * 2);
        let reachable = interactive(element)
            .then(|| element.bounds.map(|bounds| bounds.center()))
            .flatten()
            .filter(|(x, y)| self.options.clip.contains(*x, *y) && !(self.options.covered)(*x, *y));
        if let (Some(point), Marks::Refs { first }) = (reachable, self.options.marks) {
            let _ = write!(self.out, "[e{}] ", first + self.points.len() as u32);
            self.points.push(point);
        }
        self.out.push_str(&element.role);
        let name = shown(&element.name);
        if !name.is_empty() {
            let _ = write!(self.out, " \"{name}\"");
        }
        if let Some(text) = element
            .text
            .as_deref()
            .map(shown)
            .filter(|text| *text != name)
        {
            let _ = write!(self.out, " =\"{text}\"");
        }
        if let Some(target) = element.target.as_deref() {
            let _ = write!(self.out, " -> {}", shown(target));
        }
        let states = &element.states;
        let mut flags = Vec::new();
        if states.focused {
            flags.push("focused");
        }
        if states.checked {
            flags.push("checked");
        }
        if states.selected {
            flags.push("selected");
        }
        if states.pressed {
            flags.push("pressed");
        }
        if states.expandable {
            flags.push(if states.expanded {
                "expanded"
            } else {
                "collapsed"
            });
        }
        if interactive(element) {
            if !states.enabled {
                flags.push("disabled");
            }
            match element.bounds.map(|bounds| bounds.center()) {
                Some((x, y)) if !self.options.clip.contains(x, y) => flags.push("off-screen"),
                Some((x, y)) if (self.options.covered)(x, y) => flags.push("covered"),
                Some((x, y)) => {
                    if !flags.is_empty() {
                        let _ = write!(self.out, " [{}]", flags.join(", "));
                    }
                    if self.options.marks == Marks::Points {
                        self.points.push((x, y));
                        let _ = write!(self.out, " @{x},{y}");
                    }
                    self.out.push('\n');
                    return;
                }
                None => flags.push("no position"),
            }
        }
        if !flags.is_empty() {
            let _ = write!(self.out, " [{}]", flags.join(", "));
        }
        self.out.push('\n');
    }

    fn walk(&mut self, element: &Element, depth: usize, parent_name: &str) {
        if NOISE.contains(&element.role.as_str()) {
            return;
        }
        let repeats = LABELS.contains(&element.role.as_str())
            && !parent_name.is_empty()
            && element.name.trim() == parent_name.trim();
        if repeats {
            return;
        }
        let next = if folds(element) {
            depth
        } else {
            self.line(element, depth);
            depth + 1
        };
        let name = if folds(element) {
            parent_name
        } else {
            &element.name
        };
        for child in &element.children {
            self.walk(child, next, name);
        }
    }
}

/// The outline of a window's tree; see the module documentation for what is written.
pub fn write(root: &Element, options: &Options<'_>) -> Outline {
    let mut writer = Writer {
        options,
        points: Vec::new(),
        out: String::new(),
        lines: 0,
        left_out: 0,
    };
    for child in &root.children {
        writer.walk(child, 0, &root.name);
    }
    if writer.left_out > 0 {
        let _ = writeln!(writer.out, "… {} more controls not shown", writer.left_out);
    }
    Outline {
        text: writer.out,
        points: writer.points,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::element::States;

    fn element(role: &str, name: &str, at: Option<(i32, i32)>, children: Vec<Element>) -> Element {
        Element {
            role: role.to_string(),
            name: name.to_string(),
            text: None,
            target: None,
            states: States {
                showing: true,
                enabled: true,
                ..States::default()
            },
            bounds: at.map(|(x, y)| Rect {
                x: x - 10,
                y: y - 10,
                w: 20,
                h: 20,
            }),
            children,
        }
    }

    fn outline(root: &Element, covered: &dyn Fn(i32, i32) -> bool, lines: usize) -> String {
        marked(root, covered, lines, Marks::Points).text
    }

    fn marked(
        root: &Element,
        covered: &dyn Fn(i32, i32) -> bool,
        lines: usize,
        marks: Marks,
    ) -> Outline {
        write(
            root,
            &Options {
                marks,
                clip: Rect {
                    x: 0,
                    y: 0,
                    w: 1000,
                    h: 1000,
                },
                covered,
                lines,
            },
        )
    }

    #[test]
    fn a_nameless_container_gives_its_place_to_what_it_holds() {
        let root = element(
            "frame",
            "App",
            None,
            vec![element(
                "panel",
                "",
                None,
                vec![element("button", "Save", Some((50, 60)), vec![])],
            )],
        );
        assert_eq!(
            outline(&root, &|_, _| false, 100),
            "button \"Save\" @50,60\n"
        );
    }

    #[test]
    fn a_label_carries_no_point_and_a_repeated_one_is_dropped() {
        let root = element(
            "frame",
            "App",
            None,
            vec![
                element("label", "Name:", Some((20, 20)), vec![]),
                element(
                    "button",
                    "Open",
                    Some((50, 60)),
                    vec![element("static", "Open", Some((50, 60)), vec![])],
                ),
            ],
        );
        assert_eq!(
            outline(&root, &|_, _| false, 100),
            "label \"Name:\"\nbutton \"Open\" @50,60\n"
        );
    }

    #[test]
    fn a_point_a_click_would_not_reach_is_marked_instead_of_given() {
        let mut far = element("button", "Far", Some((5000, 60)), vec![]);
        far.states.checked = true;
        let root = element(
            "frame",
            "App",
            None,
            vec![far, element("button", "Under", Some((50, 60)), vec![])],
        );
        assert_eq!(
            outline(&root, &|x, _| x == 50, 100),
            "button \"Far\" [checked, off-screen]\nbutton \"Under\" [covered]\n"
        );
    }

    #[test]
    fn the_text_a_control_holds_follows_its_name() {
        let mut entry = element("entry", "Address", Some((50, 60)), vec![]);
        entry.text = Some("example.com".to_string());
        entry.states.focused = true;
        let root = element("frame", "App", None, vec![entry]);
        assert_eq!(
            outline(&root, &|_, _| false, 100),
            "entry \"Address\" =\"example.com\" [focused] @50,60\n"
        );
    }

    #[test]
    fn a_model_is_given_reference_numbers_and_the_points_stay_behind() {
        let root = element(
            "frame",
            "App",
            None,
            vec![
                element("label", "Name:", Some((20, 20)), vec![]),
                element("button", "Open", Some((50, 60)), vec![]),
                element("button", "Under", Some((70, 60)), vec![]),
                element("button", "Save", Some((90, 60)), vec![]),
            ],
        );
        let outline = marked(&root, &|x, _| x == 70, 100, Marks::Refs { first: 41 });
        assert_eq!(
            outline.text,
            "label \"Name:\"\n[e41] button \"Open\"\nbutton \"Under\" [covered]\n[e42] button \"Save\"\n"
        );
        assert_eq!(outline.points, [(50, 60), (90, 60)]);
    }

    #[test]
    fn a_link_says_where_it_goes() {
        let mut link = element("link", "Learn more", Some((50, 60)), vec![]);
        link.target = Some("https://iana.org/help".to_string());
        let root = element("frame", "App", None, vec![link]);
        assert_eq!(
            outline(&root, &|_, _| false, 100),
            "link \"Learn more\" -> https://iana.org/help @50,60\n"
        );
    }

    #[test]
    fn an_outline_past_its_budget_says_what_it_left_out() {
        let buttons = (0..5)
            .map(|i| element("button", &format!("B{i}"), Some((50, 60)), vec![]))
            .collect();
        let root = element("frame", "App", None, buttons);
        let text = outline(&root, &|_, _| false, 2);
        assert_eq!(text.lines().count(), 3);
        assert!(text.ends_with("… 3 more controls not shown\n"));
    }
}
