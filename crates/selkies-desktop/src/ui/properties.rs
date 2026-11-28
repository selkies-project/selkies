/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The properties window of a file or folder: a toplevel of the shell's own that the Properties
//! row of the file manager's and the desktop icons' right-click menus opens, one per item.
//!
//! Under the item's icon and name it lists its type, where the item is, its size, the target of a
//! link, when it was modified, accessed, and created, its permissions, and its owner and group,
//! all read from the item's metadata as the window opens; a folder adds what it holds and an
//! image its dimensions. Those two take reading more than the item's own metadata (every entry
//! of the tree, or the image's header), so they are measured on the file manager's inspection
//! worker ([`crate::files::ops`]) and shown as "Calculating…" until the result arrives. The
//! window's height is fixed by the rows it opens with, so a result fills a row rather than
//! adding one. Several windows can be open at once, each for its own item, and opening one for
//! an item whose window is already open opens a fresh one in its place with what the item
//! holds now.

use std::os::unix::fs::MetadataExt;
use std::path::Path;

use tiny_skia::Pixmap;

use crate::describe::{self, Element};
use crate::desktop::{Desktop, Geometry, Hover};
use crate::entries;
use crate::event::{Button, Request};
use crate::files::ops::Inspection;
use crate::files::{self, Item};
use crate::geom::Rect;
use crate::i18n::{tr, trf};
use crate::render::{Align, Canvas};
use crate::ui;
use crate::xkb::Key;

/// Width of the window.
const WIDTH: i32 = 460;
/// Inner padding.
const PAD: i32 = 18;
/// Side of the item's icon.
const ICON: i32 = 48;
/// Height of a row of the list.
const ROW: i32 = 26;
/// Width of the rows' labels.
const LABEL_W: i32 = 112;
/// Height of the Close button.
const CONTROL: i32 = 32;
/// Width of the Close button.
const BUTTON_W: i32 = 96;
/// The window's icon.
const ICON_NAME: &str = "document-properties";

/// One properties window.
pub struct PropertiesWindow {
    /// The window's number.
    pub id: u32,
    pub geometry: Geometry,
    pub pix: Option<Pixmap>,
    pub dirty: bool,
    /// The item, for its icon and name.
    item: Item,
    /// The item's type, as the file manager's Type column names it, with the type itself.
    kind: String,
    /// What the item's metadata says, read as the window opened.
    stat: Stat,
    /// Whether the item is an image whose dimensions are asked for.
    image: bool,
    /// What the inspection worker measured, once it has.
    inspection: Option<Inspection>,
    /// The window's title.
    title: String,
}

/// The parts of an item's metadata the window shows.
#[derive(Clone, Debug, Default, PartialEq)]
struct Stat {
    /// The size in bytes of a file, or of what a link points to.
    size: u64,
    /// Where a link points.
    link_target: Option<String>,
    /// Seconds since the epoch.
    modified: i64,
    /// Seconds since the epoch.
    accessed: i64,
    /// Seconds since the epoch, where the file system records it.
    created: Option<i64>,
    /// The permission bits, with set-user-ID, set-group-ID, and sticky.
    mode: u32,
    uid: u32,
    gid: u32,
}

impl Stat {
    /// Reads an item's metadata: a link's own for where it points, else what it points to, or
    /// the link's own where that is gone.
    fn read(path: &Path) -> Option<Stat> {
        let own = std::fs::symlink_metadata(path).ok()?;
        let link_target = own
            .file_type()
            .is_symlink()
            .then(|| std::fs::read_link(path).ok())
            .flatten()
            .map(|target| target.display().to_string());
        let metadata = std::fs::metadata(path).unwrap_or(own);
        let created = metadata
            .created()
            .ok()
            .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|since| i64::try_from(since.as_secs()).ok());
        Some(Stat {
            size: metadata.len(),
            link_target,
            modified: metadata.mtime(),
            accessed: metadata.atime(),
            created,
            mode: metadata.mode() & 0o7777,
            uid: metadata.uid(),
            gid: metadata.gid(),
        })
    }
}

/// Opens the properties window of an item, replacing one already open for it.
///
/// # Arguments
/// * `d` - The desktop.
/// * `item` - The item, as the file manager or the desktop listed it.
pub fn open(d: &mut Desktop, item: &Item) {
    if let Some(open) = d
        .properties
        .iter()
        .find(|w| w.item.path == item.path)
        .map(|w| w.id)
    {
        close(d, open);
    }
    let Some(stat) = Stat::read(&item.path) else {
        return;
    };
    let kind = if item.is_dir {
        tr("Folder").to_string()
    } else {
        let comment = d.files.mime.comment(&item.mime);
        match comment {
            Some(comment) => format!("{comment} ({})", item.mime),
            None => item.mime.clone(),
        }
    };
    let image = !item.is_dir && item.mime.starts_with("image/");
    let id = d.files.allocate_id();
    if item.is_dir || image {
        d.files.worker.inspect(id, item.path.clone(), image);
    }
    let title = trf("{name} Properties", &[("name", &item.name)]);
    let window = PropertiesWindow {
        id,
        geometry: Geometry::default(),
        pix: None,
        dirty: true,
        item: item.clone(),
        kind,
        stat,
        image,
        inspection: None,
        title: title.clone(),
    };
    let height = height_for(window.rows().len());
    d.properties.push(window);
    let icon = d.icons.get(ICON_NAME, 48).map(|icon| Box::new((*icon).clone()));
    d.requests.push(Request::OpenWindow {
        id,
        title,
        width: WIDTH,
        height,
        app_id: entries::FILES_APP_ID,
        icon_name: ICON_NAME,
        icon,
    });
}

/// Closes a properties window.
pub fn close(d: &mut Desktop, id: u32) {
    d.properties.retain(|w| w.id != id);
    d.requests.push(Request::CloseWindow { id });
    if d.hover == Hover::Properties(id) {
        d.set_hover(Hover::None);
    }
}

/// Hands the measurements the inspection worker finished to their windows; a window closed
/// since it asked drops its result.
pub fn receive(d: &mut Desktop) {
    for (id, inspection) in std::mem::take(&mut d.files.inspected) {
        if let Some(window) = d.properties.iter_mut().find(|w| w.id == id) {
            window.inspection = Some(inspection);
            window.dirty = true;
        }
    }
}

/// The window's height for a number of rows.
fn height_for(rows: usize) -> i32 {
    let rows = i32::try_from(rows).unwrap_or(i32::MAX / ROW);
    list_top() + rows * ROW + PAD + CONTROL + PAD
}

/// Where the list of rows starts, under the header and its separator.
fn list_top() -> i32 {
    PAD + ICON + 2 * PAD
}

/// The Close button, along the bottom right.
fn close_rect(geometry: Geometry) -> Rect {
    Rect::new(
        geometry.width - PAD - BUTTON_W,
        geometry.height - PAD - CONTROL,
        BUTTON_W,
        CONTROL,
    )
}

impl PropertiesWindow {
    /// The rows of the list, each a label and its value.
    fn rows(&self) -> Vec<(&'static str, String)> {
        let pending = || tr("Calculating…").to_string();
        let stat = &self.stat;
        let mut rows = vec![
            (tr("Type"), self.kind.clone()),
            (tr("Location"), location(&self.item.path)),
        ];
        if let Some(target) = &stat.link_target {
            rows.push((tr("Link target"), target.clone()));
        }
        if self.item.is_dir {
            let contents = self.inspection.and_then(|i| i.contents);
            rows.push((tr("Size"), contents.map_or_else(pending, |c| size_text(c.bytes))));
            rows.push((
                tr("Contents"),
                contents.map_or_else(pending, |c| {
                    trf(
                        "Files: {files}, folders: {folders}",
                        &[("files", &grouped(c.files)), ("folders", &grouped(c.folders))],
                    )
                }),
            ));
        } else {
            rows.push((tr("Size"), size_text(stat.size)));
        }
        if self.image {
            let value = match self.inspection {
                None => pending(),
                Some(Inspection {
                    dimensions: Some((width, height)),
                    ..
                }) => trf(
                    "{width} × {height} pixels",
                    &[("width", &width.to_string()), ("height", &height.to_string())],
                ),
                Some(_) => tr("Unknown").to_string(),
            };
            rows.push((tr("Dimensions"), value));
        }
        rows.push((tr("Modified"), when(stat.modified)));
        rows.push((tr("Accessed"), when(stat.accessed)));
        if let Some(created) = stat.created {
            rows.push((tr("Created"), when(created)));
        }
        rows.push((
            tr("Permissions"),
            format!("{} ({:o})", mode_text(stat.mode), stat.mode),
        ));
        rows.push((tr("Owner"), account(user_name(stat.uid), stat.uid)));
        rows.push((tr("Group"), account(group_name(stat.gid), stat.gid)));
        rows
    }
}

/// The folder an item is in.
fn location(path: &Path) -> String {
    path.parent()
        .map(|parent| parent.display().to_string())
        .unwrap_or_else(|| "/".to_string())
}

/// A size both short and exact: `1.5 MB (1,523,456 bytes)`.
fn size_text(bytes: u64) -> String {
    trf(
        "{size} ({bytes} bytes)",
        &[("size", &files::human_size(bytes)), ("bytes", &grouped(bytes))],
    )
}

/// A count with its thousands grouped by commas.
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// A time in full, in the locale's own format.
fn when(secs: i64) -> String {
    crate::clock::format_time(secs, "%c")
}

/// Permission bits as `ls -l` writes them, the set-ID and sticky bits in the execute places.
fn mode_text(mode: u32) -> String {
    let bit = |mask: u32, c: char| if mode & mask != 0 { c } else { '-' };
    let exec = |mask: u32, special: u32, set: char| match (mode & mask != 0, mode & special != 0) {
        (true, true) => set,
        (false, true) => set.to_ascii_uppercase(),
        (true, false) => 'x',
        (false, false) => '-',
    };
    [
        bit(0o400, 'r'),
        bit(0o200, 'w'),
        exec(0o100, 0o4000, 's'),
        bit(0o040, 'r'),
        bit(0o020, 'w'),
        exec(0o010, 0o2000, 's'),
        bit(0o004, 'r'),
        bit(0o002, 'w'),
        exec(0o001, 0o1000, 't'),
    ]
    .into_iter()
    .collect()
}

/// An account as its name and number, or the number alone where no name is known.
fn account(name: Option<String>, id: u32) -> String {
    match name {
        Some(name) => format!("{name} ({id})"),
        None => id.to_string(),
    }
}

/// The name of a user, from the system's account database.
fn user_name(uid: u32) -> Option<String> {
    let mut buf = vec![0u8; 1024];
    loop {
        // SAFETY: an all-zero `passwd` is a valid value of the C struct, which `getpwuid_r` fills.
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: every pointer is to a live local, and `buf.len()` is the buffer's true size.
        let rc = unsafe { libc::getpwuid_r(uid, &mut entry, buf.as_mut_ptr().cast(), buf.len(), &mut found) };
        if rc == libc::ERANGE && buf.len() < 1 << 20 {
            buf.resize(buf.len() * 4, 0);
            continue;
        }
        if rc != 0 || found.is_null() || entry.pw_name.is_null() {
            return None;
        }
        // SAFETY: `pw_name` points into `buf`, a C string `getpwuid_r` terminated.
        let name = unsafe { std::ffi::CStr::from_ptr(entry.pw_name) };
        return Some(name.to_string_lossy().into_owned());
    }
}

/// The name of a group, from the system's group database.
fn group_name(gid: u32) -> Option<String> {
    let mut buf = vec![0u8; 1024];
    loop {
        // SAFETY: an all-zero `group` is a valid value of the C struct, which `getgrgid_r` fills.
        let mut entry: libc::group = unsafe { std::mem::zeroed() };
        let mut found: *mut libc::group = std::ptr::null_mut();
        // SAFETY: every pointer is to a live local, and `buf.len()` is the buffer's true size.
        let rc = unsafe { libc::getgrgid_r(gid, &mut entry, buf.as_mut_ptr().cast(), buf.len(), &mut found) };
        if rc == libc::ERANGE && buf.len() < 1 << 20 {
            buf.resize(buf.len() * 4, 0);
            continue;
        }
        if rc != 0 || found.is_null() || entry.gr_name.is_null() {
            return None;
        }
        // SAFETY: `gr_name` points into `buf`, a C string `getgrgid_r` terminated.
        let name = unsafe { std::ffi::CStr::from_ptr(entry.gr_name) };
        return Some(name.to_string_lossy().into_owned());
    }
}

/// The window of a number.
fn window(d: &Desktop, id: u32) -> Option<&PropertiesWindow> {
    d.properties.iter().find(|w| w.id == id)
}

/// Marks a window for drawing.
fn redraw(d: &mut Desktop, id: u32) {
    if let Some(window) = d.properties.iter_mut().find(|w| w.id == id) {
        window.dirty = true;
    }
}

/// Follows the pointer for the Close button's hover.
pub fn pointer_motion(d: &mut Desktop, id: u32, x: f64, y: f64) {
    let Some(geometry) = window(d, id).map(|w| w.geometry) else {
        return;
    };
    let hover = if close_rect(geometry).contains(x, y) {
        Hover::Properties(id)
    } else {
        Hover::None
    };
    if hover != d.hover {
        d.set_hover(hover);
        redraw(d, id);
    }
}

/// Clears the hover when the pointer leaves.
pub fn pointer_leave(d: &mut Desktop, id: u32) {
    if d.hover == Hover::Properties(id) {
        d.set_hover(Hover::None);
        redraw(d, id);
    }
}

/// Handles a button: Close closes the window.
pub fn pointer_button(d: &mut Desktop, id: u32, x: f64, y: f64, button: Button, pressed: bool) {
    if pressed || button != Button::Left {
        return;
    }
    if window(d, id).is_some_and(|w| close_rect(w.geometry).contains(x, y)) {
        close(d, id);
    }
}

/// Handles a key: Escape and Enter close the window.
pub fn key(d: &mut Desktop, id: u32, key: Key) {
    if matches!(key, Key::Escape | Key::Enter) {
        close(d, id);
    }
}

/// Records the size the window manager gave a window.
pub fn configure(d: &mut Desktop, id: u32, geometry: Geometry) {
    if let Some(window) = d.properties.iter_mut().find(|w| w.id == id) {
        window.geometry = geometry;
        window.pix = None;
        window.dirty = true;
    }
}

/// The rectangle of the nth row's label and of its value.
fn row_rects(geometry: Geometry, row: usize) -> (Rect, Rect) {
    let y = list_top() + i32::try_from(row).unwrap_or(0) * ROW;
    let label = Rect::new(PAD, y, LABEL_W, ROW);
    let value = Rect::new(PAD + LABEL_W + 8, y, geometry.width - 2 * PAD - LABEL_W - 8, ROW);
    (label, value)
}

/// The header's name, beside the icon.
fn name_rect(geometry: Geometry) -> Rect {
    let x = PAD + ICON + 14;
    Rect::new(x, PAD, geometry.width - x - PAD, ICON)
}

/// A window's controls, as [`crate::describe`] reports them: the name, each row's label and
/// value, and the Close button.
pub fn describe(d: &Desktop, id: u32) -> Option<Element> {
    let w = window(d, id)?;
    let geometry = w.geometry;
    let mut children = vec![describe::node("label", w.item.name.clone(), name_rect(geometry))];
    for (row, (label, value)) in w.rows().into_iter().enumerate() {
        let (label_rect, value_rect) = row_rects(geometry, row);
        children.push(describe::node("label", label, label_rect));
        children.push(describe::node("label", value, value_rect));
    }
    children.push(describe::node("button", tr("Close"), close_rect(geometry)));
    Some(describe::frame(&w.title, geometry, children))
}

/// Draws a window: the icon and name, a separator, the rows, and the Close button.
pub fn draw(d: &mut Desktop, id: u32, canvas: &mut Canvas<'_>) {
    let Some(w) = window(d, id) else {
        return;
    };
    let geometry = w.geometry;
    let item = w.item.clone();
    let rows = w.rows();
    let hovered = d.hover == Hover::Properties(id);
    canvas.fill_rect(Rect::new(0, 0, geometry.width, geometry.height), ui::popup_bg());
    let px = u32::try_from(ICON * geometry.scale.max(1)).unwrap_or(48);
    if let Some(icon) = ui::files::item_icon_px(d, &item, px) {
        canvas.draw_image_fit(&icon, Rect::new(PAD, PAD, ICON, ICON));
    }
    ui::label(
        canvas,
        &mut d.fonts,
        &item.name,
        name_rect(geometry),
        ui::FONT_SIZE + 2.0,
        true,
        Align::Left,
        ui::text(),
    );
    canvas.fill_rect(
        Rect::new(PAD, PAD + ICON + PAD - 1, geometry.width - 2 * PAD, 1),
        ui::separator(),
    );
    for (row, (label, value)) in rows.iter().enumerate() {
        let (label_rect, value_rect) = row_rects(geometry, row);
        ui::label(
            canvas,
            &mut d.fonts,
            label,
            label_rect,
            ui::FONT_SIZE,
            false,
            Align::Left,
            ui::text_dim(),
        );
        ui::label(
            canvas,
            &mut d.fonts,
            value,
            value_rect,
            ui::FONT_SIZE,
            false,
            Align::Left,
            ui::text(),
        );
    }
    let button = close_rect(geometry);
    canvas.fill_rounded(button, 7.0, ui::accent());
    if hovered {
        canvas.fill_rounded(button, 7.0, ui::text_alpha(0x22));
    }
    ui::label(
        canvas,
        &mut d.fonts,
        tr("Close"),
        button,
        ui::FONT_SIZE,
        true,
        Align::Center,
        ui::on_accent(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Permission bits read as `ls -l` writes them, the set-ID and sticky bits shown in the
    /// execute places, lowercase where the execute bit is set too.
    #[test]
    fn permissions_read_as_ls_writes_them() {
        assert_eq!(mode_text(0o644), "rw-r--r--");
        assert_eq!(mode_text(0o755), "rwxr-xr-x");
        assert_eq!(mode_text(0o4755), "rwsr-xr-x");
        assert_eq!(mode_text(0o2644), "rw-r-Sr--");
        assert_eq!(mode_text(0o1777), "rwxrwxrwt");
        assert_eq!(mode_text(0o1776), "rwxrwxrwT");
    }

    /// Counts group their thousands, and short ones stay as they are.
    #[test]
    fn counts_group_their_thousands() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1000), "1,000");
        assert_eq!(grouped(1_523_456), "1,523,456");
    }

    /// A file's metadata gives its size and permissions, and a link names where it points
    /// while the size is its target's.
    #[test]
    fn a_link_names_its_target_and_takes_its_size() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("file.txt");
        std::fs::write(&file, b"hello").expect("write");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).expect("chmod");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&file, &link).expect("symlink");
        let stat = Stat::read(&file).expect("stat");
        assert_eq!((stat.size, stat.mode, stat.link_target), (5, 0o640, None));
        let linked = Stat::read(&link).expect("stat");
        assert_eq!(linked.size, 5);
        assert_eq!(linked.link_target, Some(file.display().to_string()));
    }

    /// The window is tall enough for its rows, the Close button below the last.
    #[test]
    fn the_window_fits_its_rows_above_the_button() {
        let rows = 10;
        let geometry = Geometry {
            width: WIDTH,
            height: height_for(rows),
            scale: 1,
            configured: true,
        };
        let (_, last) = row_rects(geometry, rows - 1);
        let button = close_rect(geometry);
        assert!(last.bottom() < button.y);
        assert_eq!(button.bottom(), geometry.height - PAD);
    }
}
