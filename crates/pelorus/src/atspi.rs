/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The accessibility bus: what the session's applications say is inside their windows.
//!
//! AT-SPI is a second D-Bus bus whose address the session bus hands out (`org.a11y.Bus`).
//! Every application that takes part registers a tree of objects there, each answering for
//! its role, name, states, children, and extents. Who takes part differs: GTK always does; Qt
//! and Gecko do once `org.a11y.Status.IsEnabled` is true, a property they watch, so [`enable`]
//! sets it and they join within seconds without a restart or an environment variable.
//! Chromium and everything built on it (Chrome, Electron) read the property once, as they
//! start, and register nothing where it was false, so a session turns accessibility on before
//! it starts its applications; even then they register one bare frame and fill it only when
//! started with `--force-renderer-accessibility`, which [`Miss::Bare`] reports so a caller can
//! say what to do about it.
//!
//! A read walks one window's tree with a call per fact, a few per control, descending only
//! into what is showing and stopping at a budget, since a web page can hold many thousands of
//! objects. Every call carries a timeout: an application that hangs costs its window's read,
//! never the reader. Calls block ([`zbus::block_on`]), so an async caller reads on a blocking
//! thread.

use std::time::Duration;

use zbus::connection::Builder;
use zbus::{Connection, OwnedObjectPath, OwnedVariant, Variant};

use crate::element::{Element, Placement, Protocol, Rect, States, Toolkit};

const BUS_NAME: &str = "org.a11y.Bus";
const BUS_PATH: &str = "/org/a11y/bus";
const REGISTRY: &str = "org.a11y.atspi.Registry";
const ROOT: &str = "/org/a11y/atspi/accessible/root";
const ACCESSIBLE: &str = "org.a11y.atspi.Accessible";
const APPLICATION: &str = "org.a11y.atspi.Application";
const COMPONENT: &str = "org.a11y.atspi.Component";
const TEXT: &str = "org.a11y.atspi.Text";
const HYPERLINK: &str = "org.a11y.atspi.Hyperlink";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";

/// How long one call to an application may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(1);

/// The most characters read of a text control.
const TEXT_LIMIT: i32 = 400;

/// The deepest a tree is followed.
const DEPTH_LIMIT: usize = 48;

/// `ATSPI_COORD_TYPE_SCREEN` and `ATSPI_COORD_TYPE_WINDOW`.
const COORD_SCREEN: u32 = 0;
const COORD_WINDOW: u32 = 1;

/// The name of each role by its number (`AtspiRole`), the vocabulary every toolkit's controls
/// are written in. A toolkit's own name for a role (`GetRoleName`) is not that: GTK 4 answers
/// `text box` and `generic` where the others answer `text` and `panel`.
const ROLES: &[&str] = &[
    "invalid",
    "accelerator label",
    "alert",
    "animation",
    "arrow",
    "calendar",
    "canvas",
    "check box",
    "check menu item",
    "color chooser",
    "column header",
    "combo box",
    "date editor",
    "desktop icon",
    "desktop frame",
    "dial",
    "dialog",
    "directory pane",
    "drawing area",
    "file chooser",
    "filler",
    "focus traversable",
    "font chooser",
    "frame",
    "glass pane",
    "html container",
    "icon",
    "image",
    "internal frame",
    "label",
    "layered pane",
    "list",
    "list item",
    "menu",
    "menu bar",
    "menu item",
    "option pane",
    "page tab",
    "page tab list",
    "panel",
    "password text",
    "popup menu",
    "progress bar",
    "button",
    "radio button",
    "radio menu item",
    "root pane",
    "row header",
    "scroll bar",
    "scroll pane",
    "separator",
    "slider",
    "spin button",
    "split pane",
    "status bar",
    "table",
    "table cell",
    "table column header",
    "table row header",
    "tearoff menu item",
    "terminal",
    "text",
    "toggle button",
    "tool bar",
    "tool tip",
    "tree",
    "tree table",
    "unknown",
    "viewport",
    "window",
    "extended",
    "header",
    "footer",
    "paragraph",
    "ruler",
    "application",
    "autocomplete",
    "editbar",
    "embedded",
    "entry",
    "chart",
    "caption",
    "document frame",
    "heading",
    "page",
    "section",
    "redundant object",
    "form",
    "link",
    "input method window",
    "table row",
    "tree item",
    "document spreadsheet",
    "document presentation",
    "document text",
    "document web",
    "document email",
    "comment",
    "list box",
    "grouping",
    "image map",
    "notification",
    "info bar",
    "level bar",
    "title bar",
    "block quote",
    "audio",
    "video",
    "definition",
    "article",
    "landmark",
    "log",
    "marquee",
    "math",
    "rating",
    "timer",
    "static",
    "math fraction",
    "math root",
    "subscript",
    "superscript",
    "description list",
    "description term",
    "description value",
    "footnote",
    "content deletion",
    "content insertion",
    "mark",
    "suggestion",
    "push button menu",
    "switch",
];

/// The roles whose content is read through the text interface.
const TEXT_ROLES: &[&str] = &[
    "text",
    "entry",
    "password text",
    "terminal",
    "paragraph",
    "document text",
    "spin button",
    "editbar",
];

/// The roles a toplevel window's object carries.
const WINDOW_ROLES: &[&str] = &["frame", "dialog", "window", "file chooser", "alert"];

/// Why a window has no tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Miss {
    /// No application of the window's process is on the bus: its toolkit has no accessibility
    /// support, or draws everything itself.
    Absent,
    /// A Chromium-based application registered its window and nothing inside it: it was not
    /// started with `--force-renderer-accessibility`.
    Bare,
    /// The application is on the bus and none of its windows matches this one.
    Unmatched,
}

/// One object on the bus: the connection that owns it and its path there.
#[derive(Clone, Debug)]
struct Object {
    owner: String,
    path: OwnedObjectPath,
}

/// A connection to the session's accessibility bus; a clone shares the one connection, so a
/// caller holding several reads apart can each take one.
#[derive(Clone)]
pub struct Bus {
    conn: Connection,
}

/// Turn accessibility on for the session whose bus is at `session` (the caller's own session
/// bus where `None`), starting its accessibility bus where none runs yet, and return that
/// bus's address. Applications started afterwards find it on, which Chromium needs; Qt and
/// Gecko follow the change live. Fails where the session bus cannot be reached or cannot
/// start an accessibility bus (AT-SPI is not installed).
pub fn enable(session: Option<&str>) -> Result<String, String> {
    zbus::block_on(async {
        let builder = match session {
            Some(address) => Builder::address(address),
            None => Builder::session(),
        };
        let session = builder
            .method_timeout(Duration::from_secs(5))
            .build()
            .await
            .map_err(|e| format!("no session bus: {e}"))?;
        let reply = session
            .call_method(Some(BUS_NAME), BUS_PATH, Some(BUS_NAME), "GetAddress", &())
            .await
            .map_err(|e| format!("no accessibility bus: {e}"))?;
        let address: String = reply.body().deserialize().map_err(|e| e.to_string())?;
        session
            .call_method(
                Some(BUS_NAME),
                BUS_PATH,
                Some(PROPERTIES),
                "Set",
                &("org.a11y.Status", "IsEnabled", Variant::from(true)),
            )
            .await
            .map_err(|e| format!("accessibility could not be turned on: {e}"))?;
        Ok(address)
    })
}

impl Bus {
    /// Connect to the accessibility bus of the session bus at `session`, turning accessibility
    /// on as [`enable`] does. Fails as it does, or where the accessibility bus refuses.
    pub fn connect(session: Option<&str>) -> Result<Bus, String> {
        let address = enable(session)?;
        zbus::block_on(async {
            let conn = Builder::address(address.as_str())
                .method_timeout(CALL_TIMEOUT)
                .build()
                .await
                .map_err(|e| format!("the accessibility bus refused the connection: {e}"))?;
            Ok(Bus { conn })
        })
    }

    async fn call<B, R>(
        &self,
        object: &Object,
        interface: &str,
        method: &str,
        body: &B,
    ) -> Option<R>
    where
        B: serde::Serialize + zbus::DynamicType,
        R: serde::de::DeserializeOwned + zbus::Type,
    {
        let reply = self
            .conn
            .call_method(
                Some(object.owner.as_str()),
                object.path.as_str(),
                Some(interface),
                method,
                body,
            )
            .await
            .ok()?;
        reply.body().deserialize::<R>().ok()
    }

    async fn property(&self, object: &Object, interface: &str, name: &str) -> Option<OwnedVariant> {
        self.call(object, PROPERTIES, "Get", &(interface, name))
            .await
    }

    async fn text_property(&self, object: &Object, interface: &str, name: &str) -> String {
        self.property(object, interface, name)
            .await
            .and_then(|value| String::try_from(value).ok())
            .unwrap_or_default()
    }

    async fn children(&self, object: &Object) -> Vec<Object> {
        self.call::<_, Vec<(String, OwnedObjectPath)>>(object, ACCESSIBLE, "GetChildren", &())
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|(owner, path)| Object { owner, path })
            .collect()
    }

    async fn role(&self, object: &Object) -> String {
        let number: Option<u32> = self.call(object, ACCESSIBLE, "GetRole", &()).await;
        match number.and_then(|n| ROLES.get(n as usize)) {
            Some(name) => (*name).to_string(),
            None => self
                .call(object, ACCESSIBLE, "GetRoleName", &())
                .await
                .unwrap_or_default(),
        }
    }

    async fn states(&self, object: &Object) -> States {
        let words: Vec<u32> = self
            .call(object, ACCESSIBLE, "GetState", &())
            .await
            .unwrap_or_default();
        states_of(&words)
    }

    async fn extents(&self, object: &Object, coord: u32) -> Option<Rect> {
        let (x, y, w, h): (i32, i32, i32, i32) = self
            .call(object, COMPONENT, "GetExtents", &(coord,))
            .await?;
        Some(Rect { x, y, w, h })
    }

    /// The start of the text a control holds, or `None` where it holds none. The end asked for
    /// is the text's own length where that is shorter than the limit: Qt answers nothing at
    /// all for a range that runs past the end, where GTK cuts it to what there is.
    async fn text(&self, object: &Object) -> Option<String> {
        let length = self
            .property(object, TEXT, "CharacterCount")
            .await
            .and_then(|value| i32::try_from(value).ok())
            .unwrap_or(TEXT_LIMIT);
        if length <= 0 {
            return None;
        }
        self.call::<_, String>(object, TEXT, "GetText", &(0i32, length.min(TEXT_LIMIT)))
            .await
            .filter(|text| !text.trim().is_empty())
    }

    /// The process a connection on the bus belongs to.
    async fn pid_of(&self, owner: &str) -> Option<u32> {
        let reply = self
            .conn
            .call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "GetConnectionUnixProcessID",
                &(owner,),
            )
            .await
            .ok()?;
        reply.body().deserialize().ok()
    }

    /// The tree of the window of process `pid` titled `title`, placed where `placement` says,
    /// holding at most `budget` controls. The window is matched among the process's toplevel
    /// objects by its title, else by the size of its client area, else it is the only one.
    pub fn window_tree(
        &self,
        pid: u32,
        title: &str,
        placement: &Placement,
        budget: usize,
    ) -> Result<Element, Miss> {
        zbus::block_on(async {
            let registry = Object {
                owner: REGISTRY.to_string(),
                path: OwnedObjectPath::try_from(ROOT).map_err(|_| Miss::Absent)?,
            };
            let mut found_application = false;
            let mut bare = false;
            let mut candidates = Vec::new();
            for app in self.children(&registry).await {
                if self.pid_of(&app.owner).await != Some(pid) {
                    continue;
                }
                let toolkit = Toolkit::named(
                    &self.text_property(&app, APPLICATION, "ToolkitName").await,
                    &self.text_property(&app, APPLICATION, "Version").await,
                );
                for top in self.children(&app).await {
                    found_application = true;
                    let role = self.role(&top).await;
                    if !WINDOW_ROLES.contains(&role.as_str()) {
                        continue;
                    }
                    let name = self.text_property(&top, ACCESSIBLE, "Name").await;
                    let size = self.extents(&top, COORD_WINDOW).await;
                    candidates.push((top, toolkit, name, size));
                }
            }
            if candidates.is_empty() {
                return Err(if found_application {
                    Miss::Unmatched
                } else {
                    Miss::Absent
                });
            }
            let client = (
                (f64::from(placement.rect.w) / placement.scale).round() as i32,
                (f64::from(placement.rect.h) / placement.scale).round() as i32,
            );
            let pick = candidates
                .iter()
                .position(|(_, _, name, _)| !name.is_empty() && name == title)
                .or_else(|| {
                    candidates.iter().position(|(_, _, name, _)| {
                        !name.is_empty()
                            && (title.starts_with(name.as_str()) || name.starts_with(title))
                    })
                })
                .or_else(|| {
                    candidates
                        .iter()
                        .position(|(_, _, _, size)| size.is_some_and(|s| (s.w, s.h) == client))
                })
                .or((candidates.len() == 1).then_some(0))
                .ok_or(Miss::Unmatched)?;
            let (top, toolkit, _, _) = candidates.swap_remove(pick);
            if toolkit == Toolkit::Chromium && self.children(&top).await.is_empty() {
                bare = true;
            }
            if bare {
                return Err(Miss::Bare);
            }
            let mut left = budget.max(1);
            self.walk(top, toolkit, placement, 0, &mut left)
                .await
                .ok_or(Miss::Unmatched)
        })
    }

    /// One object and what shows beneath it, or `None` for one that is not showing. Boxed, as
    /// a recursive future has to be.
    fn walk<'a>(
        &'a self,
        object: Object,
        toolkit: Toolkit,
        placement: &'a Placement,
        depth: usize,
        left: &'a mut usize,
    ) -> std::pin::Pin<Box<dyn Future<Output = Option<Element>> + 'a>> {
        Box::pin(async move {
            if *left == 0 {
                return None;
            }
            let states = self.states(&object).await;
            if depth > 0 && !states.showing {
                return None;
            }
            *left -= 1;
            let role = self.role(&object).await;
            let mut name = self.text_property(&object, ACCESSIBLE, "Name").await;
            if name.trim().is_empty() {
                name = self.text_property(&object, ACCESSIBLE, "Description").await;
            }
            let bounds = match self.extents(&object, COORD_WINDOW).await {
                Some(window) if window.w > 0 && window.h > 0 => {
                    let screen = if placement.protocol == Protocol::X11 && toolkit != Toolkit::Gtk4
                    {
                        self.extents(&object, COORD_SCREEN).await.unwrap_or(window)
                    } else {
                        window
                    };
                    Some(placement.locate(toolkit, screen, window))
                }
                _ => None,
            };
            let text = if TEXT_ROLES.contains(&role.as_str()) {
                self.text(&object).await
            } else {
                None
            };
            let target = if role == "link" {
                self.call::<_, String>(&object, HYPERLINK, "GetURI", &(0i32,))
                    .await
                    .filter(|uri| !uri.trim().is_empty())
            } else {
                None
            };
            let mut children = Vec::new();
            if depth < DEPTH_LIMIT {
                for child in self.children(&object).await {
                    if *left == 0 {
                        break;
                    }
                    if let Some(element) =
                        self.walk(child, toolkit, placement, depth + 1, left).await
                    {
                        children.push(element);
                    }
                }
            }
            Some(Element {
                role,
                name,
                text,
                target,
                states,
                bounds,
                children,
            })
        })
    }
}

/// The states read from the two words of an accessibility state set, by `AtspiStateType`.
fn states_of(words: &[u32]) -> States {
    let bit = |n: usize| {
        words
            .get(n / 32)
            .is_some_and(|word| word & (1 << (n % 32)) != 0)
    };
    States {
        checked: bit(4),
        editable: bit(7),
        enabled: bit(8) || bit(24),
        expandable: bit(9),
        expanded: bit(10),
        focused: bit(12),
        pressed: bit(20),
        selected: bit(23),
        showing: bit(25),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_are_named_by_their_number() {
        assert_eq!(ROLES.len(), 131);
        assert_eq!(ROLES[43], "button");
        assert_eq!(ROLES[61], "text");
        assert_eq!(ROLES[116], "static");
        assert_eq!(ROLES[130], "switch");
    }

    #[test]
    fn a_state_set_is_read_by_its_bit_numbers() {
        let states = states_of(&[(1 << 8) | (1 << 24) | (1 << 25) | (1 << 12), 0]);
        assert!(states.enabled && states.showing && states.focused);
        assert!(!states.checked && !states.selected);
        assert_eq!(states_of(&[]), States::default());
    }

    #[test]
    fn enabled_takes_either_the_enabled_or_the_sensitive_bit() {
        assert!(states_of(&[1 << 24, 0]).enabled);
        assert!(states_of(&[1 << 8, 0]).enabled);
        assert!(!states_of(&[1 << 25, 0]).enabled);
    }
}
