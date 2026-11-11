/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A small XML tree over quick-xml for the openbox-format files: `rc.xml`, `menu.xml`, and what
//! a pipe menu prints. The files are small and read at startup or when a menu opens, so a tree
//! is simpler to walk than a stream, and namespace prefixes are dropped since openbox's files
//! carry a default namespace and labwc's none.

use quick_xml::Reader;
use quick_xml::events::Event;

/// One element with its attributes, text, and child elements, in document order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Node {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    /// The element's own text, trimmed, with every text child joined.
    pub text: String,
    pub children: Vec<Node>,
}

impl Node {
    /// The attribute's value, if present.
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The first child element with this name, case aside.
    pub fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.name.eq_ignore_ascii_case(name))
    }

    /// Every child element with this name, case aside.
    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
        self.children
            .iter()
            .filter(move |c| c.name.eq_ignore_ascii_case(name))
    }

    /// The trimmed text of the first child with this name.
    pub fn child_text(&self, name: &str) -> Option<&str> {
        self.child(name)
            .map(|c| c.text.as_str())
            .filter(|t| !t.is_empty())
    }

    /// Whether a child's text, or the element's own, reads as yes.
    pub fn yes(&self, name: &str) -> Option<bool> {
        self.child_text(name).map(truthy)
    }
}

/// Whether openbox's spelling of a boolean is true: `yes`, `true`, `on`, or `1`.
pub fn truthy(text: &str) -> bool {
    matches!(
        text.trim().to_ascii_lowercase().as_str(),
        "yes" | "true" | "on" | "1"
    )
}

/// The document's root element, or why it did not parse.
pub fn parse(text: &str) -> Result<Node, String> {
    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(false);
    reader.config_mut().expand_empty_elements = true;
    let mut stack: Vec<Node> = Vec::new();
    let mut root = None;
    loop {
        let event = reader.read_event().map_err(|e| e.to_string())?;
        match event {
            Event::Start(start) => {
                let name = local_name(&String::from_utf8_lossy(start.name().as_ref()));
                let mut attrs = Vec::new();
                for attr in start.attributes().flatten() {
                    let key = local_name(&String::from_utf8_lossy(attr.key.as_ref()));
                    let value = attr
                        .decoded_and_normalized_value(quick_xml::XmlVersion::Implicit1_0, reader.decoder())
                        .map(|v| v.to_string())
                        .unwrap_or_default();
                    attrs.push((key, value));
                }
                stack.push(Node {
                    name,
                    attrs,
                    ..Node::default()
                });
            }
            Event::End(_) => {
                let Some(mut node) = stack.pop() else {
                    return Err("unbalanced end tag".to_string());
                };
                node.text = node.text.trim().to_string();
                match stack.last_mut() {
                    Some(parent) => parent.children.push(node),
                    None => root = Some(node),
                }
            }
            Event::Text(text) => {
                if let Some(node) = stack.last_mut()
                    && let Ok(text) = text.decode()
                {
                    node.text.push_str(&text);
                }
            }
            Event::CData(data) => {
                if let Some(node) = stack.last_mut() {
                    node.text.push_str(&String::from_utf8_lossy(&data));
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    root.ok_or_else(|| "no root element".to_string())
}

/// A qualified name without its namespace prefix.
fn local_name(name: &str) -> String {
    name.rsplit(':').next().unwrap_or(name).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A namespaced openbox document parses into a tree with prefixes dropped and text trimmed.
    #[test]
    fn an_openbox_document_parses_into_a_tree() {
        let root = parse(
            r#"<?xml version="1.0"?>
<openbox_config xmlns="http://openbox.org/3.4/rc">
  <focus><focusNew>yes</focusNew></focus>
  <keyboard>
    <keybind key="A-F4"><action name="Close"/></keybind>
    <keybind key="W-e"><action name="Execute"><command>selkies-desktop --files</command></action></keybind>
  </keyboard>
</openbox_config>"#,
        )
        .unwrap();
        assert_eq!(root.name, "openbox_config");
        assert_eq!(root.child("focus").unwrap().yes("focusNew"), Some(true));
        let binds: Vec<&Node> = root
            .child("keyboard")
            .unwrap()
            .children_named("keybind")
            .collect();
        assert_eq!(binds.len(), 2);
        assert_eq!(binds[1].attr("key"), Some("W-e"));
        assert_eq!(
            binds[1].child("action").unwrap().child_text("command"),
            Some("selkies-desktop --files")
        );
        assert!(parse("<a><b></a>").is_err());
    }
}
