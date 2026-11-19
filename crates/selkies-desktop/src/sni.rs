//! The D-Bus system tray: the `org.kde.StatusNotifierWatcher` on the session
//! bus and the host that shows what registers with it, the protocol Qt,
//! Chromium and Electron, libappindicator, and every application speaking
//! Wayland natively park their tray icons through.  The XEmbed host
//! ([`crate::tray`]) keeps serving the X clients that dock a window instead;
//! an application finds the watcher first where it speaks both, so no icon
//! shows twice.
//!
//! An application calls `RegisterStatusNotifierItem` with its bus name, or
//! with an object path on its own connection as libappindicator does, and
//! the watcher records the item under that name and path, answers
//! `IsStatusNotifierHostRegistered` as true, since the shell is the host, and
//! signals the registration.  The host reads the item's properties with one
//! `GetAll`: its title, status, icon as pixels (`IconPixmap`, ARGB32 rows in
//! network byte order) or as a name in the icon theme or in a directory of
//! its own (`IconThemePath`), the attention icon a `NeedsAttention` status
//! shows instead, its tooltip, and the path of its menu.  The item's `New*`
//! signals read them again, and the bus's `NameOwnerChanged` for its owner
//! takes it away.
//!
//! The menu is a `com.canonical.dbusmenu` tree the application exports: the
//! shell fetches it whole with `GetLayout` when it opens, after the
//! `AboutToShow` that lets an application fill it lazily, draws it with its
//! own menus, and sends the `clicked` event for the entry chosen.  Clicks on
//! the icon itself call the item's `Activate`, `SecondaryActivate`,
//! `ContextMenu` for an item without a menu, and `Scroll`.
//!
//! Everything an application sends is bounded: items to [`MAX_ITEMS`],
//! texts to a length, pixmaps to a side and to the bytes their size needs,
//! an icon in its own directory to a regular file of a size, read and
//! decoded on the worker rather than the shell's loop ([`directory_icon`]),
//! and menus to [`MAX_MENU_NODES`] entries and [`MAX_MENU_DEPTH`] levels.
//! Properties and menu entries are read leniently ([`Lenient`]), so a value
//! of the wrong type drops that value rather than the item.
//!
//! The watcher's methods run on zbus's executor and only record and queue,
//! so a registration answers at once whatever the application does next.
//! Calls to the applications, which can be slow or never answer, run on a
//! worker thread of their own in the order the shell asked for them, each
//! bounded by the connection's method timeout, with a burst of property
//! reads for one item made once; the signals are read on another thread, so
//! a hung application never holds up the shell's loop or the bus.  What
//! the host learns reaches the loop through a [`mailbox`](crate::mailbox).

use std::collections::HashSet;
use std::os::fd::RawFd;
use std::pin::Pin;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::Poll;
use std::time::Duration;

use futures_core::Stream;
use tiny_skia::Pixmap;
use zbus::connection::Builder;
use zbus::export::serde::de::{Deserialize, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use zbus::fdo::RequestNameFlags;
use zbus::message::Header;
use zbus::names::BusName;
use zbus::object_server::SignalEmitter;
use zbus::{
    Connection, MatchRule, Message, MessageStream, ObjectPath, OwnedObjectPath, Signature, Type, Variant,
};

use crate::mailbox::{self, Mailbox, Post};
use crate::variant::Lenient;

/// The well-known name of the watcher.
pub const WATCHER_NAME: &str = "org.kde.StatusNotifierWatcher";
/// The object the watcher sits on.
pub const WATCHER_PATH: &str = "/StatusNotifierWatcher";
/// The interface of an item.
const ITEM_INTERFACE: &str = "org.kde.StatusNotifierItem";
/// The interface of an item's menu.
const MENU_INTERFACE: &str = "com.canonical.dbusmenu";
/// Where an item registered by bus name sits.
const DEFAULT_ITEM_PATH: &str = "/StatusNotifierItem";
/// The longest a call to an application may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);
/// Most items kept.
pub const MAX_ITEMS: usize = 64;
/// Most menu entries kept, over every level.
pub const MAX_MENU_NODES: usize = 512;
/// Deepest submenu kept.
pub const MAX_MENU_DEPTH: usize = 8;
/// Longest title, tooltip, or menu label kept, in characters.
const MAX_TEXT: usize = 128;
/// Largest side an icon pixmap may have.
const MAX_ICON_SIDE: i32 = 512;
/// The pixmap side preferred: the panel's icon at the largest output scale.
const ICON_SIZE: i32 = 64;
/// Largest icon file read from an item's own directory, in bytes.
const MAX_ICON_FILE: u64 = 16 << 20;
/// The subdirectories of an icon theme an item's directory may hold, the
/// scalable and larger ones first.
const THEME_SIZES: [&str; 9] = [
    "scalable", "256x256", "128x128", "96x96", "64x64", "48x48", "32x32", "24x24", "16x16",
];

/// What an item says about itself.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Status {
    /// Nothing worth the user's attention.
    Passive,
    /// The default.
    #[default]
    Active,
    /// Asking for the user, with its attention icon where it has one.
    NeedsAttention,
}

/// An item as the host read it.
#[derive(Clone, Debug, Default)]
pub struct Item {
    /// The bus name and object path it registered under.
    pub key: String,
    /// Its title.
    pub title: String,
    /// Its tooltip's title, else empty.
    pub tooltip: String,
    /// Its status.
    pub status: Status,
    /// The icon as pixels: those the item sent, at [`ICON_SIZE`] or the
    /// nearest it sent, else the file its name names in its own directory.
    pub pixmap: Option<Arc<Pixmap>>,
    /// The icon as a name in the icon theme, when there are no pixels.
    pub icon_name: Option<String>,
    /// Whether it exports a menu.
    pub has_menu: bool,
    /// Whether a primary click shows the menu rather than activating it.
    pub item_is_menu: bool,
}

/// How a menu entry toggles.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Toggle {
    /// It does not.
    #[default]
    None,
    /// A checkbox, checked when set.
    Check(bool),
    /// A radio button, chosen when set.
    Radio(bool),
}

/// An entry of an item's menu.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MenuNode {
    /// Its id in the application's menu.
    pub id: i32,
    /// Its label, the mnemonic marks taken out.
    pub label: String,
    /// Whether it is a separator.
    pub separator: bool,
    /// Whether it can be chosen.
    pub enabled: bool,
    /// How it toggles.
    pub toggle: Toggle,
    /// Its submenu's entries; empty for a plain entry.
    pub children: Vec<MenuNode>,
}

/// What the host tells the loop.
#[derive(Debug)]
pub enum Incoming {
    /// An item registered, or its properties changed.
    Item(Box<Item>),
    /// An item went away.
    Removed(String),
    /// An item's menu, as fetched for [`Command::OpenMenu`] or again after
    /// `AboutToShow` asked for it; empty when there is none to show.
    Menu { key: String, entries: Vec<MenuNode> },
    /// An item's menu changed, so an open one is fetched again.
    MenuChanged(String),
}

/// A call the shell makes on an item, by its key.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    /// A primary click, at a point on the screen.
    Activate { key: String, x: i32, y: i32 },
    /// A middle click.
    SecondaryActivate { key: String, x: i32, y: i32 },
    /// A secondary click on an item without a menu.
    ContextMenu { key: String, x: i32, y: i32 },
    /// The wheel, in steps.
    Scroll {
        key: String,
        delta: i32,
        horizontal: bool,
    },
    /// Fetch the menu to show it.
    OpenMenu { key: String },
    /// A submenu is about to show.
    MenuAboutToShow { key: String, id: i32 },
    /// A menu entry was chosen.
    MenuClicked { key: String, id: i32 },
}

impl Command {
    /// The item the command is for.
    fn key(&self) -> &str {
        match self {
            Command::Activate { key, .. }
            | Command::SecondaryActivate { key, .. }
            | Command::ContextMenu { key, .. }
            | Command::Scroll { key, .. }
            | Command::OpenMenu { key }
            | Command::MenuAboutToShow { key, .. }
            | Command::MenuClicked { key, .. } => key,
        }
    }
}

/// Work for the worker thread.
#[derive(Debug)]
enum Job {
    /// Read an item's properties.
    Refresh(String),
    /// Make a call for the shell.
    Call(Command),
}

/// A registered item.
#[derive(Clone, Debug)]
struct Entry {
    /// The bus name and path, as the watcher lists it.
    key: String,
    /// The unique name of the connection that registered it.
    owner: String,
    /// The name its calls go to.
    dest: String,
    /// Its object path.
    path: String,
    /// Its menu's object path, once read.
    menu: Option<String>,
}

/// What the watcher, the signal reader, and the worker share.
struct Shared {
    /// The registered items, in registration order.
    entries: Mutex<Vec<Entry>>,
    /// The way to the loop.
    post: Post<Incoming>,
    /// The worker's queue.
    jobs: Sender<Job>,
}

impl Shared {
    /// The registered items, a poisoned lock taken as it stands.
    fn entries(&self) -> MutexGuard<'_, Vec<Entry>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The registered item a key names.
    fn entry(&self, key: &str) -> Option<Entry> {
        self.entries().iter().find(|e| e.key == key).cloned()
    }

    /// Queues work for the worker.
    fn queue(&self, job: Job) {
        let _ = self.jobs.send(job);
    }
}

/// The bus name an item's calls go to and its object path, from what it
/// registered with: an object path on the caller's own connection, a bus
/// name with the default path, or a bus name followed by a path.
///
/// # Arguments
/// * `service` - The argument of `RegisterStatusNotifierItem`.
/// * `sender` - The unique name of the caller.
///
/// # Returns
/// `None` when the name or path is not a valid one.
fn item_address(service: &str, sender: &str) -> Option<(String, String)> {
    let (dest, path) = match service.find('/') {
        Some(0) => (sender, service),
        Some(at) => (&service[..at], &service[at..]),
        None => (service, DEFAULT_ITEM_PATH),
    };
    (BusName::try_from(dest).is_ok() && ObjectPath::try_from(path).is_ok())
        .then(|| (dest.to_string(), path.to_string()))
}

/// The `org.kde.StatusNotifierWatcher` interface.
struct Watcher {
    /// The state the host shares.
    shared: Arc<Shared>,
}

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl Watcher {
    /// Records an item and queues the read of its properties.
    async fn register_status_notifier_item(
        &self,
        service: &str,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        let sender = header
            .sender()
            .map(|s| s.to_string())
            .ok_or_else(|| zbus::fdo::Error::Failed("no sender".into()))?;
        let (dest, path) = item_address(service, &sender)
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs(format!("not an item: {service}")))?;
        let key = format!("{dest}{path}");
        let added = {
            let mut entries = self.shared.entries();
            let count = entries.len();
            match entries.iter_mut().find(|e| e.key == key) {
                Some(entry) => {
                    entry.owner = sender;
                    false
                }
                None if count >= MAX_ITEMS => {
                    return Err(zbus::fdo::Error::LimitsExceeded("too many items".into()));
                }
                None => {
                    entries.push(Entry {
                        key: key.clone(),
                        owner: sender,
                        dest,
                        path,
                        menu: None,
                    });
                    true
                }
            }
        };
        self.shared.queue(Job::Refresh(key.clone()));
        if added {
            let _ = Watcher::status_notifier_item_registered(&emitter, &key).await;
            let _ = self.registered_status_notifier_items_changed(&emitter).await;
        }
        Ok(())
    }

    /// Accepts another host; the shell shows every item itself.
    fn register_status_notifier_host(&self, _service: &str) {}

    /// The registered items.
    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> Vec<String> {
        self.shared.entries().iter().map(|e| e.key.clone()).collect()
    }

    /// Whether a host shows the items: the shell does.
    #[zbus(property)]
    fn is_status_notifier_host_registered(&self) -> bool {
        true
    }

    /// The version of the protocol served.
    #[zbus(property)]
    fn protocol_version(&self) -> i32 {
        0
    }

    /// An item registered.
    #[zbus(signal)]
    async fn status_notifier_item_registered(emitter: &SignalEmitter<'_>, service: &str) -> zbus::Result<()>;

    /// An item went away.
    #[zbus(signal)]
    async fn status_notifier_item_unregistered(
        emitter: &SignalEmitter<'_>,
        service: &str,
    ) -> zbus::Result<()>;

    /// A host appeared.
    #[zbus(signal)]
    async fn status_notifier_host_registered(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

/// The pixmaps of an icon: width, height, and ARGB32 rows in network byte
/// order.
type IconPixmaps = Vec<(i32, i32, Vec<u8>)>;

/// A tooltip: its icon name, its pixmaps, its title, and its description.
type ToolTip = (String, IconPixmaps, String, String);

/// The item properties the host reads, each read leniently and every other
/// one skipped unread.
#[derive(Debug, Default)]
struct ItemProps {
    title: Option<String>,
    status: Option<String>,
    icon_name: Option<String>,
    icon_pixmap: Option<IconPixmaps>,
    attention_icon_name: Option<String>,
    attention_icon_pixmap: Option<IconPixmaps>,
    icon_theme_path: Option<String>,
    menu: Option<OwnedObjectPath>,
    item_is_menu: Option<bool>,
    tool_tip: Option<ToolTip>,
}

impl Type for ItemProps {
    const SIGNATURE: &'static Signature = &zbus::signature!("a{sv}");
}

impl<'de> Deserialize<'de> for ItemProps {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<ItemProps, D::Error> {
        deserializer.deserialize_map(ItemPropsVisitor)
    }
}

/// Reads the properties dictionary key by key.
struct ItemPropsVisitor;

impl<'de> Visitor<'de> for ItemPropsVisitor {
    type Value = ItemProps;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a dictionary of item properties")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<ItemProps, A::Error> {
        let mut props = ItemProps::default();
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "Title" => props.title = map.next_value::<Lenient<String>>()?.0,
                "Status" => props.status = map.next_value::<Lenient<String>>()?.0,
                "IconName" => props.icon_name = map.next_value::<Lenient<String>>()?.0,
                "IconPixmap" => props.icon_pixmap = map.next_value::<Lenient<IconPixmaps>>()?.0,
                "AttentionIconName" => props.attention_icon_name = map.next_value::<Lenient<String>>()?.0,
                "AttentionIconPixmap" => {
                    props.attention_icon_pixmap = map.next_value::<Lenient<IconPixmaps>>()?.0;
                }
                "IconThemePath" => props.icon_theme_path = map.next_value::<Lenient<String>>()?.0,
                "Menu" => props.menu = map.next_value::<Lenient<OwnedObjectPath>>()?.0,
                "ItemIsMenu" => props.item_is_menu = map.next_value::<Lenient<bool>>()?.0,
                "ToolTip" => props.tool_tip = map.next_value::<Lenient<ToolTip>>()?.0,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(props)
    }
}

/// A text cut to [`MAX_TEXT`] characters on one line.
fn clean(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_TEXT)
        .collect::<String>()
        .trim()
        .to_string()
}

/// An icon name the theme can look up: no path, nothing empty.
fn icon_name(name: Option<String>) -> Option<String> {
    name.map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty() && n.len() <= 255 && !n.contains(['/', '\0']))
}

/// The pixmap of an icon at the size the panel draws, as premultiplied
/// pixels: the smallest at least [`ICON_SIZE`] wide, else the largest, of
/// those whose sides are 1 to [`MAX_ICON_SIDE`] and whose bytes cover them.
///
/// # Arguments
/// * `pixmaps` - The icon's pixmaps as the item sent them.
pub fn best_pixmap(pixmaps: &[(i32, i32, Vec<u8>)]) -> Option<Pixmap> {
    let valid = pixmaps.iter().filter(|(w, h, data)| {
        (1..=MAX_ICON_SIDE).contains(w)
            && (1..=MAX_ICON_SIDE).contains(h)
            && data.len() >= (*w as usize) * (*h as usize) * 4
    });
    let (w, h, data) = valid.min_by_key(|(w, _, _)| {
        if *w >= ICON_SIZE {
            *w
        } else {
            4 * MAX_ICON_SIDE - *w
        }
    })?;
    let count = (*w as usize) * (*h as usize);
    let mut rgba = Vec::with_capacity(count * 4);
    for px in data[..count * 4].as_chunks::<4>().0 {
        rgba.extend_from_slice(&[px[1], px[2], px[3], px[0]]);
    }
    crate::render::pixmap_from_rgba(*w as u32, *h as u32, &rgba)
}

/// The icon a name names in an item's own directory, `IconThemePath`: a
/// file of the name in it, else one in the `hicolor` theme it may hold, the
/// largest first.  Only regular files of at most [`MAX_ICON_FILE`] bytes are
/// read, and they are decoded here, on the worker, under the image crate's
/// allocation limits, so the shell's loop never reads a file an item named.
///
/// # Arguments
/// * `dir` - The item's directory, an absolute path.
/// * `name` - The icon name, without a path.
fn directory_icon(dir: &str, name: &str) -> Option<Pixmap> {
    let dir = std::path::Path::new(dir);
    let flat = std::iter::once(dir.to_path_buf());
    let themed = THEME_SIZES
        .iter()
        .flat_map(|size| ["apps", "status"].map(|context| dir.join("hicolor").join(size).join(context)));
    flat.chain(themed)
        .flat_map(|d| ["png", "svg", "svgz"].map(|ext| d.join(format!("{name}.{ext}"))))
        .find(|path| std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() <= MAX_ICON_FILE))
        .and_then(|path| {
            let size = ICON_SIZE as u32;
            if path.extension().is_some_and(|e| e != "png") {
                crate::render::load_svg(&path, size)
            } else {
                crate::render::load_raster(&path, size)
            }
        })
}

impl ItemProps {
    /// The item these properties describe.
    fn into_item(self, key: String) -> Item {
        let status = match self.status.as_deref() {
            Some("Passive") => Status::Passive,
            Some("NeedsAttention") => Status::NeedsAttention,
            _ => Status::Active,
        };
        let dir = self
            .icon_theme_path
            .filter(|p| p.starts_with('/') && p.len() <= 4096 && !p.contains('\0'));
        let attention = status == Status::NeedsAttention;
        let attention_pixmap = attention
            .then(|| self.attention_icon_pixmap.as_deref().and_then(best_pixmap))
            .flatten();
        let attention_name = attention.then(|| icon_name(self.attention_icon_name)).flatten();
        let (pixmap, name) = if attention_pixmap.is_some() || attention_name.is_some() {
            (attention_pixmap, attention_name)
        } else {
            (
                self.icon_pixmap.as_deref().and_then(best_pixmap),
                icon_name(self.icon_name),
            )
        };
        Item {
            key,
            title: clean(self.title.as_deref().unwrap_or_default()),
            tooltip: self
                .tool_tip
                .map(|(_, _, title, _)| clean(&title))
                .unwrap_or_default(),
            status,
            pixmap: pixmap
                .or_else(|| directory_icon(dir.as_deref()?, name.as_deref()?))
                .map(Arc::new),
            icon_name: name,
            has_menu: menu_path(self.menu.as_ref()).is_some(),
            item_is_menu: self.item_is_menu.unwrap_or(false),
        }
    }
}

/// The object path of a menu, or `None` for none: libappindicator names
/// `/NO_DBUSMENU` and others the root for an item without one.
fn menu_path(menu: Option<&OwnedObjectPath>) -> Option<String> {
    menu.map(|p| p.as_str().to_string())
        .filter(|p| p != "/" && p != "/NO_DBUSMENU")
}

/// The menu entry properties the host reads.
#[derive(Debug, Default)]
struct MenuProps {
    kind: Option<String>,
    label: Option<String>,
    enabled: Option<bool>,
    visible: Option<bool>,
    toggle_type: Option<String>,
    toggle_state: Option<i32>,
}

impl Type for MenuProps {
    const SIGNATURE: &'static Signature = &zbus::signature!("a{sv}");
}

impl<'de> Deserialize<'de> for MenuProps {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<MenuProps, D::Error> {
        deserializer.deserialize_map(MenuPropsVisitor)
    }
}

/// Reads a menu entry's properties key by key.
struct MenuPropsVisitor;

impl<'de> Visitor<'de> for MenuPropsVisitor {
    type Value = MenuProps;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a dictionary of menu entry properties")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<MenuProps, A::Error> {
        let mut props = MenuProps::default();
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "type" => props.kind = map.next_value::<Lenient<String>>()?.0,
                "label" => props.label = map.next_value::<Lenient<String>>()?.0,
                "enabled" => props.enabled = map.next_value::<Lenient<bool>>()?.0,
                "visible" => props.visible = map.next_value::<Lenient<bool>>()?.0,
                "toggle-type" => props.toggle_type = map.next_value::<Lenient<String>>()?.0,
                "toggle-state" => props.toggle_state = map.next_value::<Lenient<i32>>()?.0,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(props)
    }
}

/// The signature of a dbusmenu layout node: its id, its properties, and its
/// children, each a variant holding a node.
const LAYOUT_SIGNATURE: Signature = zbus::signature!("(ia{sv}av)");

/// A dbusmenu layout node as `GetLayout` sends it.
#[derive(Debug, Default)]
struct RawNode {
    id: i32,
    props: MenuProps,
    children: Vec<Lenient<RawNode>>,
}

impl Type for RawNode {
    const SIGNATURE: &'static Signature = &LAYOUT_SIGNATURE;
}

impl<'de> Deserialize<'de> for RawNode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<RawNode, D::Error> {
        deserializer.deserialize_struct("RawNode", &["id", "props", "children"], RawNodeVisitor)
    }
}

/// Reads a layout node's three fields.
struct RawNodeVisitor;

impl<'de> Visitor<'de> for RawNodeVisitor {
    type Value = RawNode;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a menu layout node")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<RawNode, A::Error> {
        use zbus::export::serde::de::Error;
        Ok(RawNode {
            id: seq
                .next_element()?
                .ok_or_else(|| A::Error::invalid_length(0, &self))?,
            props: seq
                .next_element()?
                .ok_or_else(|| A::Error::invalid_length(1, &self))?,
            children: seq
                .next_element()?
                .ok_or_else(|| A::Error::invalid_length(2, &self))?,
        })
    }
}

/// A label with its mnemonic marks taken out: `_` before a character marks
/// it, and `__` is a literal underscore.
pub fn strip_mnemonic(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut chars = label.chars();
    while let Some(c) = chars.next() {
        if c == '_' {
            match chars.next() {
                Some(next) => out.push(next),
                None => out.push('_'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The entries of a menu level from the application's layout: hidden
/// entries left out, separators that lead, trail, or follow another dropped,
/// and the whole bounded by `budget` entries and [`MAX_MENU_DEPTH`] levels.
///
/// # Arguments
/// * `nodes` - The level's nodes.
/// * `depth` - How deep the level is, the top one at zero.
/// * `budget` - How many entries may still be kept.
fn menu_entries(nodes: Vec<RawNode>, depth: usize, budget: &mut usize) -> Vec<MenuNode> {
    let mut entries: Vec<MenuNode> = Vec::new();
    for raw in nodes {
        if *budget == 0 {
            break;
        }
        if raw.props.visible == Some(false) {
            continue;
        }
        *budget -= 1;
        let separator = raw.props.kind.as_deref() == Some("separator");
        if separator && entries.last().is_none_or(|e| e.separator) {
            continue;
        }
        let children = if depth + 1 < MAX_MENU_DEPTH && !separator {
            let nodes = raw.children.into_iter().filter_map(|c| c.0).collect();
            menu_entries(nodes, depth + 1, budget)
        } else {
            Vec::new()
        };
        let state = raw.props.toggle_state == Some(1);
        entries.push(MenuNode {
            id: raw.id,
            label: clean(&strip_mnemonic(raw.props.label.as_deref().unwrap_or_default())),
            separator,
            enabled: raw.props.enabled.unwrap_or(true),
            toggle: match raw.props.toggle_type.as_deref() {
                Some("checkmark") => Toggle::Check(state),
                Some("radio") => Toggle::Radio(state),
                _ => Toggle::None,
            },
            children,
        });
    }
    if entries.last().is_some_and(|e| e.separator) {
        entries.pop();
    }
    entries
}

/// Reads an item's properties and hands the item to the loop.
async fn refresh(conn: &Connection, shared: &Shared, key: &str) {
    let Some(entry) = shared.entry(key) else {
        return;
    };
    let reply = conn
        .call_method(
            Some(entry.dest.as_str()),
            entry.path.as_str(),
            Some("org.freedesktop.DBus.Properties"),
            "GetAll",
            &(ITEM_INTERFACE,),
        )
        .await;
    let props = match reply.and_then(|r| r.body().deserialize::<ItemProps>()) {
        Ok(props) => props,
        Err(error) => {
            debug(&format!("cannot read tray item {key}: {error}"));
            return;
        }
    };
    let menu = menu_path(props.menu.as_ref());
    {
        let mut entries = shared.entries();
        let Some(entry) = entries.iter_mut().find(|e| e.key == key) else {
            return;
        };
        entry.menu = menu;
    }
    shared
        .post
        .send(Incoming::Item(Box::new(props.into_item(key.to_string()))));
}

/// Fetches an item's menu and hands it to the loop, empty where the item
/// has none or the call failed.
async fn fetch_menu(conn: &Connection, shared: &Shared, key: &str) {
    let entries = match shared.entry(key) {
        Some(Entry {
            dest,
            menu: Some(menu),
            ..
        }) => {
            let _ = conn
                .call_method(
                    Some(dest.as_str()),
                    menu.as_str(),
                    Some(MENU_INTERFACE),
                    "AboutToShow",
                    &(0i32,),
                )
                .await;
            let reply = conn
                .call_method(
                    Some(dest.as_str()),
                    menu.as_str(),
                    Some(MENU_INTERFACE),
                    "GetLayout",
                    &(0i32, -1i32, Vec::<&str>::new()),
                )
                .await;
            match reply.and_then(|r| r.body().deserialize::<(u32, RawNode)>()) {
                Ok((_, root)) => {
                    let nodes = root.children.into_iter().filter_map(|c| c.0).collect();
                    let mut budget = MAX_MENU_NODES;
                    menu_entries(nodes, 0, &mut budget)
                }
                Err(error) => {
                    debug(&format!("cannot read the menu of tray item {key}: {error}"));
                    Vec::new()
                }
            }
        }
        _ => Vec::new(),
    };
    shared.post.send(Incoming::Menu {
        key: key.to_string(),
        entries,
    });
}

/// Makes a call the shell asked for.
async fn call(conn: &Connection, shared: &Shared, command: Command) {
    let Some(entry) = shared.entry(command.key()) else {
        return;
    };
    let item = |method: &'static str, x: i32, y: i32| {
        let (dest, path) = (entry.dest.clone(), entry.path.clone());
        async move {
            conn.call_method(
                Some(dest.as_str()),
                path.as_str(),
                Some(ITEM_INTERFACE),
                method,
                &(x, y),
            )
            .await
        }
    };
    let result = match command {
        Command::Activate { x, y, .. } => item("Activate", x, y).await.map(drop),
        Command::SecondaryActivate { x, y, .. } => item("SecondaryActivate", x, y).await.map(drop),
        Command::ContextMenu { x, y, .. } => item("ContextMenu", x, y).await.map(drop),
        Command::Scroll {
            delta, horizontal, ..
        } => {
            let orientation = if horizontal { "horizontal" } else { "vertical" };
            conn.call_method(
                Some(entry.dest.as_str()),
                entry.path.as_str(),
                Some(ITEM_INTERFACE),
                "Scroll",
                &(delta, orientation),
            )
            .await
            .map(drop)
        }
        Command::OpenMenu { key } => {
            fetch_menu(conn, shared, &key).await;
            Ok(())
        }
        Command::MenuAboutToShow { key, id } => match entry.menu.as_deref() {
            Some(menu) => {
                let reply = conn
                    .call_method(
                        Some(entry.dest.as_str()),
                        menu,
                        Some(MENU_INTERFACE),
                        "AboutToShow",
                        &(id,),
                    )
                    .await;
                if reply
                    .and_then(|r| r.body().deserialize::<bool>())
                    .unwrap_or(false)
                {
                    fetch_menu(conn, shared, &key).await;
                }
                Ok(())
            }
            None => Ok(()),
        },
        Command::MenuClicked { id, .. } => match entry.menu.as_deref() {
            Some(menu) => conn
                .call_method(
                    Some(entry.dest.as_str()),
                    menu,
                    Some(MENU_INTERFACE),
                    "Event",
                    &(id, "clicked", Variant::I32(0), 0u32),
                )
                .await
                .map(drop),
            None => Ok(()),
        },
    };
    if let Err(error) = result {
        debug(&format!("tray item {} did not take a call: {error}", entry.key));
    }
}

/// The worker: runs the queued jobs in order, reading a burst of property
/// changes for one item once.
fn work(conn: &Connection, shared: &Shared, jobs: &Receiver<Job>) {
    while let Ok(first) = jobs.recv() {
        let batch: Vec<Job> = std::iter::once(first).chain(jobs.try_iter()).collect();
        let mut refreshed = HashSet::new();
        for job in batch {
            match job {
                Job::Refresh(key) => {
                    if refreshed.insert(key.clone()) {
                        zbus::block_on(refresh(conn, shared, &key));
                    }
                }
                Job::Call(command) => zbus::block_on(call(conn, shared, command)),
            }
        }
    }
}

/// The match rules of the signals the host reads: owners leaving the bus,
/// items' changes, and their menus' changes.
fn signal_rules() -> zbus::Result<[MatchRule<'static>; 3]> {
    let signal = || MatchRule::builder().msg_type(zbus::message::Type::Signal);
    Ok([
        signal()
            .sender("org.freedesktop.DBus")
            .interface("org.freedesktop.DBus")
            .member("NameOwnerChanged")
            .build()?,
        signal().interface(ITEM_INTERFACE).build()?,
        signal().interface(MENU_INTERFACE).build()?,
    ])
}

/// Acts on a signal: an owner that left takes its items with it, an item's
/// `New*` signal reads it again, and a menu's change tells the loop.
async fn signal(conn: &Connection, shared: &Shared, msg: &Message) {
    let header = msg.header();
    let (Some(interface), Some(member)) = (header.interface(), header.member()) else {
        return;
    };
    let sender = header.sender().map(|s| s.as_str()).unwrap_or_default();
    let path = header.path().map(|p| p.as_str()).unwrap_or_default();
    match interface.as_str() {
        "org.freedesktop.DBus" if member.as_str() == "NameOwnerChanged" => {
            let Ok((name, _, new)) = msg.body().deserialize::<(String, String, String)>() else {
                return;
            };
            if !new.is_empty() {
                return;
            }
            let removed: Vec<String> = {
                let mut entries = shared.entries();
                let (gone, kept): (Vec<Entry>, Vec<Entry>) =
                    entries.drain(..).partition(|e| e.owner == name || e.dest == name);
                *entries = kept;
                gone.into_iter().map(|e| e.key).collect()
            };
            if removed.is_empty() {
                return;
            }
            if let Ok(emitter) = SignalEmitter::new(conn, WATCHER_PATH) {
                for key in &removed {
                    let _ = Watcher::status_notifier_item_unregistered(&emitter, key).await;
                }
            }
            if let Ok(watcher) = conn.object_server().interface::<_, Watcher>(WATCHER_PATH).await {
                let _ = watcher
                    .get()
                    .await
                    .registered_status_notifier_items_changed(watcher.signal_emitter())
                    .await;
            }
            for key in removed {
                shared.post.send(Incoming::Removed(key));
            }
        }
        ITEM_INTERFACE if member.as_str().starts_with("New") => {
            let key = shared
                .entries()
                .iter()
                .find(|e| e.owner == sender && e.path == path)
                .map(|e| e.key.clone());
            if let Some(key) = key {
                shared.queue(Job::Refresh(key));
            }
        }
        MENU_INTERFACE if matches!(member.as_str(), "LayoutUpdated" | "ItemsPropertiesUpdated") => {
            let key = shared
                .entries()
                .iter()
                .find(|e| e.owner == sender && e.menu.as_deref() == Some(path))
                .map(|e| e.key.clone());
            if let Some(key) = key {
                shared.post.send(Incoming::MenuChanged(key));
            }
        }
        _ => {}
    }
}

/// Reads the signals until the connection closes.
fn listen(conn: &Connection, shared: &Shared, mut streams: Vec<MessageStream>) {
    zbus::block_on(async {
        loop {
            let next = std::future::poll_fn(|cx| {
                for stream in streams.iter_mut() {
                    if let Poll::Ready(message) = Pin::new(stream).poll_next(cx) {
                        return Poll::Ready(message);
                    }
                }
                Poll::Pending
            })
            .await;
            match next {
                Some(Ok(msg)) => signal(conn, shared, &msg).await,
                Some(Err(_)) => {}
                None => break,
            }
        }
    });
}

/// Prints a line under `SELKIES_DESKTOP_DEBUG`.
fn debug(line: &str) {
    if std::env::var_os("SELKIES_DESKTOP_DEBUG").is_some() {
        eprintln!("selkies-desktop: {line}");
    }
}

/// The watcher and host on the session bus, served while this value lives.
pub struct Host {
    /// What the host learned.
    mailbox: Mailbox<Incoming>,
    /// The worker's queue.
    jobs: Sender<Job>,
    /// The connection, holding the name.
    _conn: Connection,
}

impl Host {
    /// Serves the watcher on the session bus, or returns `None`, with a line
    /// under `SELKIES_DESKTOP_DEBUG`, when there is no bus or another
    /// process holds the name.
    pub fn start() -> Option<Host> {
        match Host::serve(Builder::session()) {
            Ok(host) => Some(host),
            Err(error) => {
                debug(&format!("not hosting tray items: {error}"));
                None
            }
        }
    }

    /// Serves the watcher on the bus a builder connects to and starts the
    /// signal reader and the worker.
    ///
    /// # Errors
    /// The pipe's, the connection's, or a thread's failure, or
    /// [`zbus::Error::NameTaken`] when the name has an owner.
    fn serve(builder: Builder<'_>) -> zbus::Result<Host> {
        let io = |e: std::io::Error| zbus::Error::InputOutput(Arc::new(e));
        let (post, mailbox) = mailbox::channel().map_err(io)?;
        let (jobs, queue) = channel();
        let shared = Arc::new(Shared {
            entries: Mutex::new(Vec::new()),
            post,
            jobs: jobs.clone(),
        });
        let watcher = Watcher {
            shared: Arc::clone(&shared),
        };
        let (conn, streams) = zbus::block_on(async {
            let conn = builder
                .method_timeout(CALL_TIMEOUT)
                .serve_at(WATCHER_PATH, watcher)
                .build()
                .await?;
            let mut streams = Vec::new();
            for rule in signal_rules()? {
                streams.push(MessageStream::for_match_rule(rule, &conn, None).await?);
            }
            conn.request_name_with_flags(WATCHER_NAME, RequestNameFlags::DO_NOT_QUEUE)
                .await?;
            let emitter = SignalEmitter::new(&conn, WATCHER_PATH)?;
            let _ = Watcher::status_notifier_host_registered(&emitter).await;
            zbus::Result::Ok((conn, streams))
        })?;
        let (listen_conn, listen_shared) = (conn.clone(), Arc::clone(&shared));
        std::thread::Builder::new()
            .name("tray-signals".into())
            .spawn(move || listen(&listen_conn, &listen_shared, streams))
            .map_err(io)?;
        let work_conn = conn.clone();
        std::thread::Builder::new()
            .name("tray-calls".into())
            .spawn(move || work(&work_conn, &shared, &queue))
            .map_err(io)?;
        Ok(Host {
            mailbox,
            jobs,
            _conn: conn,
        })
    }

    /// The descriptor that becomes readable when something arrived.
    pub fn fd(&self) -> RawFd {
        self.mailbox.fd()
    }

    /// Takes everything that arrived, clearing the wake pipe.
    pub fn drain(&self) -> Vec<Incoming> {
        self.mailbox.drain()
    }

    /// Queues a call on an item.
    pub fn send(&self, command: Command) {
        let _ = self.jobs.send(Job::Call(command));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;
    use std::sync::mpsc::RecvTimeoutError;

    use zbus::OwnedVariant;

    use crate::test_bus::Bus;

    #[test]
    fn an_item_registers_by_bus_name_by_its_own_path_or_by_both() {
        assert_eq!(
            item_address("org.kde.StatusNotifierItem-7-1", ":1.9"),
            Some((
                "org.kde.StatusNotifierItem-7-1".into(),
                "/StatusNotifierItem".into()
            ))
        );
        assert_eq!(
            item_address("/org/ayatana/NotificationItem/app", ":1.9"),
            Some((":1.9".into(), "/org/ayatana/NotificationItem/app".into()))
        );
        assert_eq!(
            item_address(":1.9/Tray/item", ":1.4"),
            Some((":1.9".into(), "/Tray/item".into()))
        );
        assert_eq!(item_address("not a name", ":1.9"), None);
        assert_eq!(item_address("/bad//path", ":1.9"), None);
    }

    #[test]
    fn the_pixmap_nearest_the_panels_size_is_taken_and_turned_from_argb() {
        let small = (2, 2, [255u8, 200, 100, 50].repeat(4));
        let large = (128, 128, vec![255u8; 128 * 128 * 4]);
        let exact = (64, 64, [128u8, 255, 0, 0].repeat(64 * 64));
        let short = (100, 100, vec![0u8; 10]);
        let picked = best_pixmap(&[small.clone(), large, exact, short.clone()]).unwrap();
        assert_eq!(picked.width(), 64, "the smallest at least the panel's size");
        assert_eq!(
            &picked.data()[..4],
            &[128, 0, 0, 128],
            "ARGB red at half alpha, premultiplied"
        );
        let picked = best_pixmap(&[small, short]).unwrap();
        assert_eq!(picked.width(), 2, "else the largest whose bytes cover it");
        assert_eq!(&picked.data()[..4], &[200, 100, 50, 255]);
        assert!(best_pixmap(&[(0, 4, vec![]), (600, 1, vec![0; 2400])]).is_none());
    }

    #[test]
    fn an_icon_named_in_the_items_own_directory_is_read_there_and_bounded() {
        let dir = std::env::temp_dir().join(format!("selkies-sni-{}", std::process::id()));
        let themed = dir.join("hicolor/48x48/apps");
        std::fs::create_dir_all(&themed).unwrap();
        let png = |w: u32| {
            let mut bytes = Vec::new();
            image::RgbaImage::from_pixel(w, w, image::Rgba([0, 0, 255, 255]))
                .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
                .unwrap();
            bytes
        };
        std::fs::write(themed.join("app-tray.png"), png(48)).unwrap();
        std::fs::write(dir.join("flat.png"), png(16)).unwrap();
        let root = dir.to_str().unwrap();
        let found = directory_icon(root, "app-tray").expect("found in the theme it holds");
        assert_eq!(found.width(), ICON_SIZE as u32);
        assert!(directory_icon(root, "flat").is_some(), "found beside the theme");
        assert!(directory_icon(root, "missing").is_none());
        let props = ItemProps {
            icon_name: Some("app-tray".into()),
            icon_theme_path: Some(root.into()),
            ..ItemProps::default()
        };
        let item = props.into_item("k".into());
        assert!(item.pixmap.is_some(), "the host reads it, never the shell's loop");
        let relative = ItemProps {
            icon_name: Some("app-tray".into()),
            icon_theme_path: Some("relative/dir".into()),
            ..ItemProps::default()
        };
        assert!(relative.into_item("k".into()).pixmap.is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn mnemonic_marks_are_taken_out_of_labels() {
        assert_eq!(strip_mnemonic("_Show Window"), "Show Window");
        assert_eq!(strip_mnemonic("Save __as_"), "Save _as_");
        assert_eq!(strip_mnemonic("Plain"), "Plain");
    }

    /// A tray item on a connection of the test's own, telling the test
    /// what it was called with.
    struct FakeItem {
        calls: Sender<String>,
    }

    #[zbus::interface(name = "org.kde.StatusNotifierItem")]
    impl FakeItem {
        #[zbus(property)]
        fn title(&self) -> String {
            "Downloads".into()
        }

        #[zbus(property)]
        fn status(&self) -> String {
            "Active".into()
        }

        #[zbus(property)]
        fn icon_pixmap(&self) -> IconPixmaps {
            vec![(1, 1, vec![255, 10, 20, 30])]
        }

        #[zbus(property)]
        fn menu(&self) -> OwnedObjectPath {
            OwnedObjectPath::try_from("/MenuBar").unwrap()
        }

        #[zbus(property)]
        fn tool_tip(&self) -> u32 {
            7
        }

        fn activate(&self, x: i32, y: i32) {
            let _ = self.calls.send(format!("Activate {x} {y}"));
        }
    }

    /// A layout node as an application sends it.
    type Node = (i32, HashMap<String, OwnedVariant>, Vec<OwnedVariant>);

    /// A layout node with properties and children.
    fn node(id: i32, props: &[(&str, Variant<'static>)], children: Vec<Node>) -> Node {
        let props = props
            .iter()
            .map(|(k, v)| (k.to_string(), OwnedVariant::try_from(v).unwrap()))
            .collect();
        let children = children
            .into_iter()
            .map(|c| OwnedVariant::try_from(Variant::from(c)).unwrap())
            .collect();
        (id, props, children)
    }

    /// An item's menu, telling the test which entries were clicked.
    struct FakeMenu {
        calls: Sender<String>,
    }

    #[zbus::interface(name = "com.canonical.dbusmenu")]
    impl FakeMenu {
        fn about_to_show(&self, id: i32) -> bool {
            let _ = self.calls.send(format!("AboutToShow {id}"));
            false
        }

        fn get_layout(&self, _parent: i32, _depth: i32, _props: Vec<String>) -> (u32, Node) {
            let label = |text: &'static str| ("label", Variant::from(text));
            let separator = ("type", Variant::from("separator"));
            let root = node(
                0,
                &[],
                vec![
                    node(1, std::slice::from_ref(&separator), vec![]),
                    node(2, &[label("_Show")], vec![]),
                    node(3, &[label("Hidden"), ("visible", Variant::from(false))], vec![]),
                    node(4, std::slice::from_ref(&separator), vec![]),
                    node(5, std::slice::from_ref(&separator), vec![]),
                    node(
                        6,
                        &[label("More"), ("enabled", Variant::from(7u32))],
                        vec![node(
                            7,
                            &[
                                label("Notify"),
                                ("toggle-type", Variant::from("checkmark")),
                                ("toggle-state", Variant::from(1i32)),
                            ],
                            vec![],
                        )],
                    ),
                    node(8, &[separator], vec![]),
                ],
            );
            (3, root)
        }

        fn event(&self, id: i32, event_id: String, _data: OwnedVariant, _timestamp: u32) {
            let _ = self.calls.send(format!("Event {id} {event_id}"));
        }
    }

    /// Waits for what the host hands the loop until one matches.
    fn receive(host: &Host, wanted: impl Fn(&Incoming) -> bool) -> Incoming {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            let mut fd = libc::pollfd {
                fd: host.fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            unsafe { libc::poll(&mut fd, 1, 200) };
            if let Some(found) = host.drain().into_iter().find(&wanted) {
                return found;
            }
        }
        panic!("nothing wanted arrived");
    }

    /// The next call the fake item or menu saw.
    fn next_call(calls: &Receiver<String>) -> String {
        match calls.recv_timeout(Duration::from_secs(10)) {
            Ok(call) => call,
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => panic!("no call arrived"),
        }
    }

    #[test]
    fn an_item_registers_shows_its_menu_takes_calls_and_leaves_with_its_owner() {
        let Some(bus) = Bus::start() else {
            eprintln!("skipped: no dbus-daemon");
            return;
        };
        let host = Host::serve(bus.builder()).unwrap();
        let (calls, seen) = channel();
        let app = zbus::block_on(
            bus.builder()
                .serve_at(DEFAULT_ITEM_PATH, FakeItem { calls: calls.clone() })
                .serve_at("/MenuBar", FakeMenu { calls })
                .build(),
        )
        .unwrap();
        let name = app.unique_name().unwrap().to_string();
        zbus::block_on(app.call_method(
            Some(WATCHER_NAME),
            WATCHER_PATH,
            Some(WATCHER_NAME),
            "RegisterStatusNotifierItem",
            &(name.as_str(),),
        ))
        .unwrap();
        let hosted: bool = zbus::block_on(app.call_method(
            Some(WATCHER_NAME),
            WATCHER_PATH,
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &(WATCHER_NAME, "IsStatusNotifierHostRegistered"),
        ))
        .unwrap()
        .body()
        .deserialize::<Lenient<bool>>()
        .unwrap()
        .0
        .unwrap();
        assert!(hosted, "applications see a host and use the watcher");

        let Incoming::Item(item) = receive(&host, |i| matches!(i, Incoming::Item(_))) else {
            unreachable!();
        };
        let key = format!("{name}{DEFAULT_ITEM_PATH}");
        assert_eq!(item.key, key);
        assert_eq!(item.title, "Downloads");
        assert_eq!(
            item.tooltip, "",
            "a tooltip of the wrong type is dropped, not the item"
        );
        assert!(item.has_menu && !item.item_is_menu);
        assert_eq!(
            item.pixmap.as_ref().map(|p| p.data().to_vec()),
            Some(vec![10, 20, 30, 255])
        );

        host.send(Command::OpenMenu { key: key.clone() });
        assert_eq!(next_call(&seen), "AboutToShow 0");
        let Incoming::Menu { entries, .. } = receive(&host, |i| matches!(i, Incoming::Menu { .. })) else {
            unreachable!();
        };
        let labels: Vec<(&str, bool)> = entries.iter().map(|e| (e.label.as_str(), e.separator)).collect();
        assert_eq!(
            labels,
            [("Show", false), ("", true), ("More", false)],
            "hidden entries and stray separators are left out"
        );
        assert!(
            entries[2].enabled,
            "an enabled flag of the wrong type keeps the default"
        );
        assert_eq!(entries[2].children[0].toggle, Toggle::Check(true));

        host.send(Command::MenuClicked {
            key: key.clone(),
            id: 7,
        });
        assert_eq!(next_call(&seen), "Event 7 clicked");
        host.send(Command::Activate {
            key: key.clone(),
            x: 12,
            y: 34,
        });
        assert_eq!(next_call(&seen), "Activate 12 34");

        drop(app);
        assert!(matches!(
            receive(&host, |i| matches!(i, Incoming::Removed(_))),
            Incoming::Removed(k) if k == key
        ));
        assert!(matches!(Host::serve(bus.builder()), Err(zbus::Error::NameTaken)));
    }
}
