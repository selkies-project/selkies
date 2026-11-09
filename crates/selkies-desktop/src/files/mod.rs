//! The file manager's model: the folders its windows show, how they are
//! listed and sorted, the places in the sidebar, the clipboard shared by
//! every window, the thumbnails, the watch that refreshes a folder when it
//! changes, and the background workers.

pub mod mime;
pub mod ops;

use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tiny_skia::Pixmap;

use crate::config;
use crate::desktop::Geometry;
use crate::i18n::{tr, trf};

pub use mime::MimeDb;
pub use ops::{Message, ThumbKey, Worker};

/// How long after the last change in a folder its listing is read again.
const RESCAN_SETTLE: Duration = Duration::from_millis(250);
/// How long a status message stays before the item count returns.
pub const STATUS_LINGER: Duration = Duration::from_secs(4);
/// Default window width.
pub const DEFAULT_WIDTH: i32 = 860;
/// Default window height.
pub const DEFAULT_HEIGHT: i32 = 560;

/// One file or folder in a listing.
#[derive(Clone, Debug)]
pub struct Item {
    /// File name.
    pub name: String,
    /// Full path.
    pub path: PathBuf,
    /// Whether the item is a directory, following symlinks.
    pub is_dir: bool,
    /// Whether the item is a symlink.
    pub is_symlink: bool,
    /// Size in bytes, zero for directories.
    pub size: u64,
    /// Modification time, seconds since the epoch.
    pub modified: i64,
    /// Mime type.
    pub mime: String,
    /// Whether the name marks the item hidden.
    pub hidden: bool,
    /// Whether the file may be executed.
    pub executable: bool,
}

/// What a listing is sorted by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    /// By name, folders first.
    Name,
    /// By size.
    Size,
    /// By modification time.
    Modified,
    /// By type.
    Kind,
}

impl SortKey {
    /// The column label.
    pub fn label(self) -> &'static str {
        match self {
            SortKey::Name => tr("Name"),
            SortKey::Size => tr("Size"),
            SortKey::Modified => tr("Modified"),
            SortKey::Kind => tr("Type"),
        }
    }

    /// The "sort by" menu text for the key.
    pub fn sort_label(self) -> &'static str {
        match self {
            SortKey::Name => tr("Sort by name"),
            SortKey::Size => tr("Sort by size"),
            SortKey::Modified => tr("Sort by date"),
            SortKey::Kind => tr("Sort by type"),
        }
    }
}

/// How a listing is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    /// A grid of large icons.
    Icons,
    /// Rows with columns.
    List,
}

/// A quick link in the sidebar.
#[derive(Clone, Debug)]
pub struct Place {
    /// Display name.
    pub name: String,
    /// The directory.
    pub path: PathBuf,
    /// Icon name.
    pub icon: &'static str,
}

/// What a picker window is choosing for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PickTarget {
    /// A wallpaper image, written to a config key.
    Wallpaper { key: &'static str, monitor: Option<u32> },
    /// The start button's icon.
    StartIcon,
}

/// The settings new windows start with.
#[derive(Clone, Copy, Debug)]
pub struct Defaults {
    /// The view.
    pub view: View,
    /// The sort key.
    pub sort: SortKey,
    /// Whether hidden files are shown.
    pub show_hidden: bool,
}

/// A window opened to choose a file rather than to manage them.
#[derive(Clone, Debug)]
pub struct Picker {
    /// What the chosen file is for.
    pub target: PickTarget,
    /// Mime types offered; directories are always shown.
    pub mimes: Vec<&'static str>,
}

/// The files cut or copied, awaiting a paste.
#[derive(Clone, Debug)]
pub struct Clipboard {
    /// The files.
    pub paths: Vec<PathBuf>,
    /// Whether the paste moves rather than copies.
    pub cut: bool,
}

/// A press that may become a click or a drag.
#[derive(Clone, Copy, Debug)]
pub struct Press {
    /// Where the button went down.
    pub x: f64,
    /// Where the button went down.
    pub y: f64,
    /// The item under it, if any.
    pub item: Option<usize>,
    /// Whether the item was already selected, so a release without a drag
    /// narrows the selection to it.
    pub was_selected: bool,
}

/// An area of a window that scrolls on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollArea {
    /// The places in the sidebar.
    Sidebar,
    /// The folder's items.
    Content,
}

/// An item being renamed.
#[derive(Clone, Debug)]
pub struct Rename {
    /// The item.
    pub index: usize,
    /// The text typed so far.
    pub text: String,
}

/// An action offered by the context menu.
#[derive(Clone, Debug, PartialEq)]
pub enum MenuAction {
    /// Open the item or the selection.
    Open,
    /// Open the selection with a desktop entry.
    OpenWith(String),
    /// Open a new window on the folder.
    OpenInNewWindow,
    /// Cut the selection.
    Cut,
    /// Copy the selection.
    Copy,
    /// Paste into the folder.
    Paste,
    /// Rename the item.
    Rename,
    /// Delete the selection after confirmation.
    Delete,
    /// Create a folder.
    NewFolder,
    /// Toggle hidden files.
    ToggleHidden,
    /// Switch the view.
    View(View),
    /// Sort by a key.
    Sort(SortKey),
    /// Open the folder in a terminal.
    Terminal,
}

/// One row of the context menu.
#[derive(Clone, Debug, PartialEq)]
pub enum MenuRow {
    /// A clickable action.
    Action(String, MenuAction, bool),
    /// A separator line.
    Separator,
}

/// The context menu open in a window.
#[derive(Clone, Debug)]
pub struct ContextMenu {
    /// Where it was opened, in window coordinates.
    pub x: i32,
    /// Where it was opened, in window coordinates.
    pub y: i32,
    /// Its rows.
    pub rows: Vec<MenuRow>,
}

/// A deletion waiting for confirmation.
#[derive(Clone, Debug)]
pub struct Confirm {
    /// The files.
    pub paths: Vec<PathBuf>,
}

/// Where a drag over the window would drop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropTarget {
    /// Into the folder shown.
    Folder,
    /// Into a folder item.
    Item(usize),
    /// Into a sidebar place.
    Place(usize),
}

/// An operation reported in the status bar.
#[derive(Clone, Debug)]
pub struct Progress {
    /// Files done.
    pub done: usize,
    /// Files in total.
    pub total: usize,
    /// The file being worked on.
    pub current: String,
}

/// One file manager window.
pub struct FilesWindow {
    /// The desktop's number for the window.
    pub id: u32,
    /// The folder shown.
    pub dir: PathBuf,
    /// Every item of the folder as read.
    all: Vec<Item>,
    /// The items shown, after the hidden filter and the typed filter, sorted.
    pub items: Vec<Item>,
    /// Selected rows of `items`.
    pub selection: HashSet<usize>,
    /// The row keyboard navigation and range selection start from.
    pub cursor: Option<usize>,
    /// Scroll offset of the content area.
    pub scroll: f64,
    /// Scroll offset of the sidebar.
    pub sidebar_scroll: f64,
    /// The view.
    pub view: View,
    /// The sort key.
    pub sort: SortKey,
    /// Whether the sort is ascending.
    pub ascending: bool,
    /// Whether hidden files are shown.
    pub show_hidden: bool,
    /// The typed filter.
    pub filter: String,
    /// Folders visited, for back and forward.
    pub history: Vec<PathBuf>,
    /// Position in the history.
    pub history_pos: usize,
    /// Size and scale.
    pub geometry: Geometry,
    /// The rendered pixels.
    pub pix: Option<Pixmap>,
    /// Whether the window needs drawing.
    pub dirty: bool,
    /// A press waiting to become a click or a drag.
    pub press: Option<Press>,
    /// The rename in progress.
    pub rename: Option<Rename>,
    /// The open context menu.
    pub menu: Option<ContextMenu>,
    /// The deletion awaiting confirmation.
    pub confirm: Option<Confirm>,
    /// The picker this window is, if it is one.
    pub picker: Option<Picker>,
    /// The path being typed into the location field.
    pub path_edit: Option<String>,
    /// A message in the status bar, shown until its time.
    pub status: Option<(String, Instant)>,
    /// Where a drag over the window would drop.
    pub drop_target: Option<DropTarget>,
    /// The last click, for double clicks.
    pub last_click: Option<(u32, usize)>,
    /// The scrollbar thumb being dragged, with the offset into it grabbed.
    pub bar_drag: Option<(ScrollArea, f64)>,
    /// The inotify watch on the folder.
    watch: i32,
    /// When the folder is read again after a change.
    rescan_at: Option<Instant>,
    /// The operation running for this window.
    pub progress: Option<(u64, Progress)>,
    /// The title last sent to the window manager.
    pub shown_title: String,
}

impl FilesWindow {
    /// The items of the folder as read, before filtering.
    pub fn all_items(&self) -> &[Item] {
        &self.all
    }

    /// The selected items in listing order.
    pub fn selected_paths(&self) -> Vec<PathBuf> {
        let mut rows: Vec<usize> = self.selection.iter().copied().collect();
        rows.sort();
        rows.into_iter()
            .filter_map(|i| self.items.get(i))
            .map(|item| item.path.clone())
            .collect()
    }

    /// Whether the window may change files: a picker may not.
    pub fn editable(&self) -> bool {
        self.picker.is_none()
    }

    /// The title the window manager shows.
    pub fn title(&self) -> String {
        if let Some(picker) = &self.picker {
            return match picker.target {
                PickTarget::Wallpaper { .. } => tr("Choose a wallpaper").to_string(),
                PickTarget::StartIcon => tr("Choose a start menu icon").to_string(),
            };
        }
        display_name(&self.dir)
    }

    /// Applies the hidden flag, the typed filter and the sort to the listing,
    /// keeping the selection on the same paths where they remain.
    pub fn refilter(&mut self) {
        let selected: HashSet<PathBuf> = self.selected_paths().into_iter().collect();
        let cursor_path = self
            .cursor
            .and_then(|c| self.items.get(c))
            .map(|i| i.path.clone());
        let needle = self.filter.to_lowercase();
        let picker_mimes: Option<Vec<&'static str>> = self.picker.as_ref().map(|p| p.mimes.clone());
        let mut items: Vec<Item> = self
            .all
            .iter()
            .filter(|item| self.show_hidden || !item.hidden)
            .filter(|item| needle.is_empty() || item.name.to_lowercase().contains(&needle))
            .filter(|item| {
                picker_mimes
                    .as_ref()
                    .is_none_or(|mimes| item.is_dir || mimes.contains(&item.mime.as_str()))
            })
            .cloned()
            .collect();
        sort_items(&mut items, self.sort, self.ascending);
        self.items = items;
        self.selection = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| selected.contains(&item.path))
            .map(|(i, _)| i)
            .collect();
        self.cursor = cursor_path.and_then(|p| self.items.iter().position(|i| i.path == p));
        if let Some(rename) = &self.rename
            && rename.index >= self.items.len()
        {
            self.rename = None;
        }
        self.dirty = true;
    }
}

/// Everything the file manager keeps across its windows.
pub struct Manager {
    /// The open windows.
    pub windows: Vec<FilesWindow>,
    next_id: u32,
    /// The mime database.
    pub mime: MimeDb,
    /// The cut or copied files.
    pub clipboard: Option<Clipboard>,
    /// The background workers.
    pub worker: Worker,
    /// Decoded thumbnails; `None` for images that could not be decoded.
    thumbs: HashMap<ThumbKey, Option<Arc<Pixmap>>>,
    /// The inotify instance watching the shown folders.
    inotify: RawFd,
    /// The window a drag started from and the files it carries, while it lasts.
    pub drag: Option<(u32, Vec<PathBuf>)>,
    /// The sidebar places, read once.
    pub places: Vec<Place>,
    /// Operations in flight, by number, with the window they report to.
    operations: HashMap<u64, u32>,
}

impl Default for Manager {
    fn default() -> Manager {
        Manager::new()
    }
}

impl Manager {
    /// Loads the mime database and starts the workers.
    pub fn new() -> Manager {
        let inotify = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        Manager {
            windows: Vec::new(),
            next_id: 1,
            mime: MimeDb::load(),
            clipboard: None,
            worker: Worker::new(),
            thumbs: HashMap::new(),
            inotify,
            drag: None,
            places: places(),
            operations: HashMap::new(),
        }
    }

    /// The descriptors the main loop polls: the worker pipe and the watch.
    pub fn fds(&self) -> Vec<RawFd> {
        let mut fds = vec![self.worker.fd()];
        if self.inotify >= 0 {
            fds.push(self.inotify);
        }
        fds
    }

    /// The window with a number.
    pub fn window(&self, id: u32) -> Option<&FilesWindow> {
        self.windows.iter().find(|w| w.id == id)
    }

    /// The window with a number, mutably.
    pub fn window_mut(&mut self, id: u32) -> Option<&mut FilesWindow> {
        self.windows.iter_mut().find(|w| w.id == id)
    }

    /// The numbers of every window.
    pub fn ids(&self) -> Vec<u32> {
        self.windows.iter().map(|w| w.id).collect()
    }

    /// Creates a window on a folder, not yet given a surface.
    ///
    /// # Arguments
    /// * `dir` - The folder, or `None` for the home directory.
    /// * `picker` - What the window picks for, when it is a picker.
    /// * `defaults` - The view, sort and hidden setting to start with.
    ///
    /// # Returns
    /// The window's number.
    pub fn open(&mut self, dir: Option<PathBuf>, picker: Option<Picker>, defaults: Defaults) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        let dir = dir.filter(|d| d.is_dir()).unwrap_or_else(config::home);
        let mut window = FilesWindow {
            id,
            dir: PathBuf::new(),
            all: Vec::new(),
            items: Vec::new(),
            selection: HashSet::new(),
            cursor: None,
            scroll: 0.0,
            sidebar_scroll: 0.0,
            view: defaults.view,
            sort: defaults.sort,
            ascending: true,
            show_hidden: defaults.show_hidden,
            filter: String::new(),
            history: Vec::new(),
            history_pos: 0,
            geometry: Geometry::default(),
            pix: None,
            dirty: true,
            press: None,
            rename: None,
            menu: None,
            confirm: None,
            picker,
            path_edit: None,
            status: None,
            drop_target: None,
            last_click: None,
            bar_drag: None,
            watch: -1,
            rescan_at: None,
            progress: None,
            shown_title: String::new(),
        };
        self.load_dir(&mut window, &dir);
        window.history.push(window.dir.clone());
        self.windows.push(window);
        id
    }

    /// Forgets a window.
    pub fn close(&mut self, id: u32) {
        if let Some(index) = self.windows.iter().position(|w| w.id == id) {
            let window = self.windows.remove(index);
            self.unwatch(window.watch);
        }
    }

    /// Reads a folder into a window, replacing what it showed.  A folder that
    /// cannot be read leaves the window where it was with a message.
    fn load_dir(&mut self, window: &mut FilesWindow, dir: &Path) {
        let dir = dir.to_path_buf();
        match read_dir(&dir, &self.mime) {
            Ok(items) => {
                self.unwatch(window.watch);
                window.watch = self.watch(&dir);
                window.dir = dir;
                window.all = items;
                window.selection.clear();
                window.cursor = None;
                window.scroll = 0.0;
                window.filter.clear();
                window.rename = None;
                window.menu = None;
                window.press = None;
                window.rescan_at = None;
                window.refilter();
            }
            Err(error) => {
                window.status = Some((format!("{}: {error}", display_name(&dir)), Instant::now()));
                window.dirty = true;
            }
        }
    }

    /// Shows a folder in a window and records it in the history.
    ///
    /// # Arguments
    /// * `id` - The window.
    /// * `dir` - The folder.
    pub fn navigate(&mut self, id: u32, dir: &Path) {
        let Some(index) = self.windows.iter().position(|w| w.id == id) else {
            return;
        };
        let mut window = self.windows.remove(index);
        let before = window.dir.clone();
        self.load_dir(&mut window, dir);
        if window.dir != before {
            window.history.truncate(window.history_pos + 1);
            window.history.push(window.dir.clone());
            window.history_pos = window.history.len() - 1;
        }
        self.windows.insert(index, window);
    }

    /// Moves through a window's history.
    ///
    /// # Arguments
    /// * `id` - The window.
    /// * `by` - Steps, negative for back.
    pub fn step_history(&mut self, id: u32, by: i32) {
        let Some(index) = self.windows.iter().position(|w| w.id == id) else {
            return;
        };
        let mut window = self.windows.remove(index);
        let target = window.history_pos as i32 + by;
        if target >= 0 && (target as usize) < window.history.len() {
            window.history_pos = target as usize;
            let dir = window.history[window.history_pos].clone();
            self.load_dir(&mut window, &dir);
        }
        self.windows.insert(index, window);
    }

    /// Reads a window's folder again, keeping the selection and scroll.
    ///
    /// # Arguments
    /// * `id` - The window.
    pub fn refresh(&mut self, id: u32) {
        let Some(window) = self.windows.iter_mut().find(|w| w.id == id) else {
            return;
        };
        window.rescan_at = None;
        let dir = window.dir.clone();
        if let Ok(items) = read_dir(&dir, &self.mime) {
            window.all = items;
            window.refilter();
        } else if let Some(index) = self.windows.iter().position(|w| w.id == id) {
            let mut window = self.windows.remove(index);
            let home = config::home();
            self.load_dir(&mut window, &home);
            self.windows.insert(index, window);
        }
    }

    /// Adds an inotify watch on a folder.
    fn watch(&self, dir: &Path) -> i32 {
        if self.inotify < 0 {
            return -1;
        }
        let Ok(c_path) = CString::new(dir.as_os_str().as_bytes()) else {
            return -1;
        };
        let mask = libc::IN_CREATE
            | libc::IN_DELETE
            | libc::IN_MOVED_FROM
            | libc::IN_MOVED_TO
            | libc::IN_CLOSE_WRITE
            | libc::IN_ATTRIB
            | libc::IN_DELETE_SELF
            | libc::IN_MOVE_SELF;
        unsafe { libc::inotify_add_watch(self.inotify, c_path.as_ptr(), mask) }
    }

    /// Removes a watch unless another window still uses it.
    fn unwatch(&self, wd: i32) {
        if wd < 0 || self.inotify < 0 {
            return;
        }
        if self.windows.iter().filter(|w| w.watch == wd).count() > 1 {
            return;
        }
        unsafe {
            libc::inotify_rm_watch(self.inotify, wd);
        }
    }

    /// Reads the queued watch notifications and schedules the affected
    /// windows' rescans.
    pub fn drain_watch(&mut self) {
        if self.inotify < 0 {
            return;
        }
        let mut buf = [0u8; 4096];
        let mut changed: Vec<i32> = Vec::new();
        loop {
            let read = unsafe { libc::read(self.inotify, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if read <= 0 {
                break;
            }
            let mut offset = 0usize;
            while offset + std::mem::size_of::<libc::inotify_event>() <= read as usize {
                let event = unsafe { &*(buf.as_ptr().add(offset) as *const libc::inotify_event) };
                changed.push(event.wd);
                offset += std::mem::size_of::<libc::inotify_event>() + event.len as usize;
            }
        }
        let due = Instant::now() + RESCAN_SETTLE;
        for window in &mut self.windows {
            if changed.contains(&window.watch) {
                window.rescan_at = Some(due);
            }
        }
    }

    /// Takes the messages the workers sent and applies them: progress and
    /// results to their windows, thumbnails to the cache.
    ///
    /// # Returns
    /// Whether anything on screen changed.
    pub fn drain_worker(&mut self) -> bool {
        let messages = self.worker.drain();
        let mut changed = false;
        for message in messages {
            changed = true;
            match message {
                Message::Progress {
                    id,
                    done,
                    total,
                    current,
                } => {
                    if let Some(window_id) = self.operations.get(&id).copied()
                        && let Some(window) = self.window_mut(window_id)
                    {
                        window.progress = Some((id, Progress { done, total, current }));
                        window.dirty = true;
                    }
                }
                Message::Done { id, errors } => {
                    if let Some(window_id) = self.operations.remove(&id)
                        && let Some(window) = self.window_mut(window_id)
                    {
                        window.progress = None;
                        if let Some(first) = errors.first() {
                            let more = if errors.len() > 1 {
                                format!(
                                    " {}",
                                    trf("(and {n} more)", &[("n", &(errors.len() - 1).to_string())])
                                )
                            } else {
                                String::new()
                            };
                            window.status = Some((format!("{first}{more}"), Instant::now()));
                        }
                        window.dirty = true;
                    }
                    for window in &mut self.windows {
                        window.rescan_at = Some(Instant::now());
                    }
                }
                Message::Thumb { key, image } => {
                    self.thumbs.insert(key, image.map(Arc::new));
                    for window in &mut self.windows {
                        window.dirty = true;
                    }
                }
            }
        }
        changed
    }

    /// Runs the rescans that are due and expires old status messages.
    ///
    /// # Arguments
    /// * `now` - The current time.
    pub fn tick(&mut self, now: Instant) {
        let due: Vec<u32> = self
            .windows
            .iter()
            .filter(|w| w.rescan_at.is_some_and(|at| at <= now))
            .map(|w| w.id)
            .collect();
        for id in due {
            self.refresh(id);
        }
        for window in &mut self.windows {
            if window
                .status
                .as_ref()
                .is_some_and(|(_, since)| now.duration_since(*since) >= STATUS_LINGER)
            {
                window.status = None;
                window.dirty = true;
            }
        }
    }

    /// When `tick` next has something to do.
    pub fn next_deadline(&self) -> Option<Instant> {
        let mut deadline: Option<Instant> = None;
        for window in &self.windows {
            for candidate in [
                window.rescan_at,
                window.status.as_ref().map(|(_, since)| *since + STATUS_LINGER),
            ]
            .into_iter()
            .flatten()
            {
                deadline = Some(deadline.map_or(candidate, |d| d.min(candidate)));
            }
        }
        deadline
    }

    /// A thumbnail for an image, decoded in the background on first request.
    ///
    /// # Arguments
    /// * `item` - The image file.
    /// * `size` - Longer side in physical pixels.
    ///
    /// # Returns
    /// The thumbnail once it is ready.
    pub fn thumbnail(&mut self, item: &Item, size: u32) -> Option<Arc<Pixmap>> {
        let key = ThumbKey {
            path: item.path.clone(),
            modified: item.modified,
            size,
        };
        if let Some(cached) = self.thumbs.get(&key) {
            return cached.clone();
        }
        self.worker.request_thumbnail(key);
        None
    }

    /// Queues copying or moving files into a folder for a window.
    ///
    /// # Arguments
    /// * `window` - The window that reports the progress.
    /// * `sources` - The files.
    /// * `dest` - The folder.
    /// * `moving` - Whether to move rather than copy.
    pub fn transfer(&mut self, window: u32, sources: Vec<PathBuf>, dest: PathBuf, moving: bool) {
        if sources.is_empty() {
            return;
        }
        let id = self.worker.submit(|id| {
            if moving {
                ops::Job::Move { id, sources, dest }
            } else {
                ops::Job::Copy { id, sources, dest }
            }
        });
        self.operations.insert(id, window);
    }

    /// Queues deleting files for a window.
    ///
    /// # Arguments
    /// * `window` - The window that reports the progress.
    /// * `paths` - The files.
    pub fn delete(&mut self, window: u32, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        let id = self.worker.submit(|id| ops::Job::Delete { id, paths });
        self.operations.insert(id, window);
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
        if self.inotify >= 0 {
            unsafe {
                libc::close(self.inotify);
            }
        }
    }
}

/// Reads a folder's entries.
///
/// # Arguments
/// * `dir` - The folder.
/// * `mime` - The mime database that types the files.
///
/// # Errors
/// The folder cannot be read.
pub fn read_dir(dir: &Path, mime: &MimeDb) -> std::io::Result<Vec<Item>> {
    let read = std::fs::read_dir(dir)?;
    let hidden_list: HashSet<String> = std::fs::read_to_string(dir.join(".hidden"))
        .map(|text| text.lines().map(|l| l.trim().to_string()).collect())
        .unwrap_or_default();
    let mut items = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let path = entry.path();
        let Ok(link_metadata) = entry.metadata() else {
            continue;
        };
        let is_symlink = link_metadata.file_type().is_symlink();
        let metadata = if is_symlink {
            std::fs::metadata(&path).unwrap_or(link_metadata)
        } else {
            link_metadata
        };
        let is_dir = metadata.is_dir();
        items.push(Item {
            mime: mime.type_of(&name, is_dir),
            hidden: name.starts_with('.') || name.ends_with('~') || hidden_list.contains(&name),
            executable: !is_dir && metadata.permissions().mode() & 0o111 != 0,
            size: if is_dir { 0 } else { metadata.len() },
            modified: metadata.mtime(),
            name,
            path,
            is_dir,
            is_symlink,
        });
    }
    Ok(items)
}

/// Sorts items by a key, folders always ahead of files.
///
/// # Arguments
/// * `items` - The items.
/// * `key` - The sort key.
/// * `ascending` - Whether to sort ascending.
pub fn sort_items(items: &mut [Item], key: SortKey, ascending: bool) {
    items.sort_by(|a, b| {
        let folders = b.is_dir.cmp(&a.is_dir);
        if folders != std::cmp::Ordering::Equal {
            return folders;
        }
        let order = match key {
            SortKey::Name => natural_cmp(&a.name, &b.name),
            SortKey::Size => a.size.cmp(&b.size).then_with(|| natural_cmp(&a.name, &b.name)),
            SortKey::Modified => a
                .modified
                .cmp(&b.modified)
                .then_with(|| natural_cmp(&a.name, &b.name)),
            SortKey::Kind => a.mime.cmp(&b.mime).then_with(|| natural_cmp(&a.name, &b.name)),
        };
        if ascending { order } else { order.reverse() }
    });
}

/// Compares names case insensitively with runs of digits compared by value,
/// so `file2` sorts before `file10`.
///
/// # Arguments
/// * `a` - One name.
/// * `b` - The other name.
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let (mut i, mut j) = (0usize, 0usize);
    while i < a_chars.len() && j < b_chars.len() {
        let (ca, cb) = (a_chars[i], b_chars[j]);
        if ca.is_ascii_digit() && cb.is_ascii_digit() {
            let start_a = i;
            while i < a_chars.len() && a_chars[i].is_ascii_digit() {
                i += 1;
            }
            let start_b = j;
            while j < b_chars.len() && b_chars[j].is_ascii_digit() {
                j += 1;
            }
            let num_a: String = a_chars[start_a..i].iter().collect();
            let num_b: String = b_chars[start_b..j].iter().collect();
            let trimmed_a = num_a.trim_start_matches('0');
            let trimmed_b = num_b.trim_start_matches('0');
            let by_value = trimmed_a
                .len()
                .cmp(&trimmed_b.len())
                .then_with(|| trimmed_a.cmp(trimmed_b))
                .then_with(|| num_a.len().cmp(&num_b.len()));
            if by_value != Ordering::Equal {
                return by_value;
            }
            continue;
        }
        let la = ca.to_lowercase().next().unwrap_or(ca);
        let lb = cb.to_lowercase().next().unwrap_or(cb);
        if la != lb {
            return la.cmp(&lb);
        }
        i += 1;
        j += 1;
    }
    (a_chars.len() - i)
        .cmp(&(b_chars.len() - j))
        .then_with(|| a.cmp(b))
}

/// The sidebar places: the home directory, the XDG user directories that
/// exist, and the root of the filesystem.
pub fn places() -> Vec<Place> {
    let home = config::home();
    let mut list = vec![Place {
        name: tr("Home").to_string(),
        path: home.clone(),
        icon: "user-home",
    }];
    let user_dirs = read_user_dirs();
    for (key, fallback, icon) in [
        ("XDG_DESKTOP_DIR", "Desktop", "user-desktop"),
        ("XDG_DOCUMENTS_DIR", "Documents", "folder-documents"),
        ("XDG_DOWNLOAD_DIR", "Downloads", "folder-download"),
        ("XDG_PICTURES_DIR", "Pictures", "folder-pictures"),
        ("XDG_MUSIC_DIR", "Music", "folder-music"),
        ("XDG_VIDEOS_DIR", "Videos", "folder-videos"),
    ] {
        let path = user_dirs.get(key).cloned().unwrap_or_else(|| home.join(fallback));
        if path.is_dir() && path != home {
            list.push(Place {
                name: display_name(&path),
                path,
                icon,
            });
        }
    }
    list.push(Place {
        name: tr("Filesystem").to_string(),
        path: PathBuf::from("/"),
        icon: "drive-harddisk",
    });
    list
}

/// Reads the XDG user directories file.
fn read_user_dirs() -> HashMap<String, PathBuf> {
    let mut dirs = HashMap::new();
    let path = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| config::home().join(".config"))
        .join("user-dirs.dirs");
    let Ok(text) = std::fs::read_to_string(path) else {
        return dirs;
    };
    let home = config::home().to_string_lossy().to_string();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let value = value.trim().trim_matches('"').replace("$HOME", &home);
            dirs.insert(key.trim().to_string(), PathBuf::from(value));
        }
    }
    dirs
}

/// The folder icon of a path: the special icon of a place, else the plain folder.
///
/// # Arguments
/// * `places` - The sidebar places.
/// * `path` - The folder.
pub fn folder_icon(places: &[Place], path: &Path) -> &'static str {
    places
        .iter()
        .find(|p| p.path == path)
        .map_or("folder", |p| p.icon)
}

/// The name a folder is shown under: its file name, "Home" for the home
/// directory, and the root as a slash.
///
/// # Arguments
/// * `path` - The folder.
pub fn display_name(path: &Path) -> String {
    if path == config::home() {
        return tr("Home").to_string();
    }
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "/".to_string())
}

/// A size in bytes as a short human readable string.
///
/// # Arguments
/// * `bytes` - The size.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// A modification time as the list shows it: the time of day for today,
/// the day and month within the year, the full date otherwise.
///
/// # Arguments
/// * `secs` - Seconds since the epoch.
pub fn human_time(secs: i64) -> String {
    let now = crate::clock::now();
    let then = crate::clock::local(secs);
    if then.year == now.year && then.month == now.month && then.day == now.day {
        crate::clock::format_time(secs, "%H:%M")
    } else if then.year == now.year {
        crate::clock::format_time(secs, "%-d %b %H:%M")
    } else {
        crate::clock::format_time(secs, "%-d %b %Y")
    }
}

pub use selkies_draw::apps::file_uri;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_order() {
        use std::cmp::Ordering;
        assert_eq!(natural_cmp("file2", "file10"), Ordering::Less);
        assert_eq!(natural_cmp("File10", "file2"), Ordering::Greater);
        assert_eq!(natural_cmp("a", "B"), Ordering::Less);
        assert_eq!(natural_cmp("img007", "img7"), Ordering::Greater);
        assert_eq!(natural_cmp("same", "same"), Ordering::Equal);
    }

    #[test]
    fn folders_sort_first() {
        let item = |name: &str, is_dir: bool, size: u64| Item {
            name: name.to_string(),
            path: PathBuf::from(name),
            is_dir,
            is_symlink: false,
            size,
            modified: 0,
            mime: String::new(),
            hidden: false,
            executable: false,
        };
        let mut items = vec![
            item("b.txt", false, 5),
            item("a.txt", false, 9),
            item("z", true, 0),
        ];
        sort_items(&mut items, SortKey::Size, false);
        let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["z", "a.txt", "b.txt"]);
    }

    #[test]
    fn sizes_and_uris() {
        assert_eq!(human_size(999), "999 B");
        assert_eq!(human_size(1500), "1.5 KB");
        assert_eq!(human_size(123_456_789), "123 MB");
        assert_eq!(
            file_uri(Path::new("/home/me/My File#1.txt")),
            "file:///home/me/My%20File%231.txt"
        );
    }
}
