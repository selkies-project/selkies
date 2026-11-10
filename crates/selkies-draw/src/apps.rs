/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Desktop entries: the applications installed in the XDG data directories, the launchers on
//! the user's desktop, their menu categories, and how an entry's command line is built. The
//! shell's start menu and the window manager's applications menu both read them here, so the
//! two list the same applications under the same categories with the same icons, and an entry
//! installed in the session is in both the next time either opens.
//!
//! The applications directories are read in precedence order, the user's data home first, then
//! each system data directory, the first file of a desktop file ID winning, so a file under the
//! home or `/usr/local/share` hides or replaces a packaged one. Entries that are not
//! applications, are `NoDisplay` or `Hidden`, name a `TryExec` that is not installed, run a
//! program that exists but may not be executed (how the hardened images disable terminals), or
//! need a terminal when none can run are left out; `OnlyShowIn` and `NotShowIn` are ignored,
//! since neither reader is a desktop environment of its own. An entry's category is the first
//! of [`CATEGORY_MAP`] its `Categories` list names, in the map's order, else [`MISC`].

use std::collections::HashSet;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::dirs;

/// Desktop entry categories folded into the menu's categories.
pub const CATEGORY_MAP: [(&str, &str); 16] = [
    ("AudioVideo", "Multimedia"),
    ("Audio", "Multimedia"),
    ("Video", "Multimedia"),
    ("Development", "Development"),
    ("Education", "Education"),
    ("Game", "Games"),
    ("Graphics", "Graphics"),
    ("Network", "Internet"),
    ("WebBrowser", "Internet"),
    ("Office", "Office"),
    ("Science", "Science"),
    ("Settings", "Settings"),
    ("System", "System"),
    ("Utility", "Utilities"),
    ("TerminalEmulator", "System"),
    ("FileManager", "System"),
];

/// The category of entries no mapping claims.
pub const MISC: &str = "Other";

/// The icon theme's name for a menu category's icon.
pub fn category_icon(name: &str) -> &'static str {
    match name {
        "Multimedia" => "applications-multimedia",
        "Development" => "applications-development",
        "Education" => "applications-education",
        "Games" => "applications-games",
        "Graphics" => "applications-graphics",
        "Internet" => "applications-internet",
        "Office" => "applications-office",
        "Science" => "applications-science",
        "Settings" => "preferences-desktop",
        "System" => "applications-system",
        "Utilities" => "applications-utilities",
        _ => "applications-other",
    }
}

/// Terminal emulators tried for entries that need one, in order.
const TERMINALS: [&str; 10] = [
    "x-terminal-emulator",
    "foot",
    "xterm",
    "st",
    "stterm",
    "lxterminal",
    "xfce4-terminal",
    "konsole",
    "gnome-terminal",
    "weston-terminal",
];

/// One launchable desktop entry.
#[derive(Clone, Debug)]
pub struct Entry {
    /// The desktop file ID, its file name within the applications directory.
    pub id: String,
    /// Localized display name.
    pub name: String,
    /// Localized comment, shown as a tooltip.
    pub comment: Option<String>,
    /// Localized generic name, searched along with the name.
    pub generic_name: Option<String>,
    /// Keywords, searched along with the name.
    pub keywords: Vec<String>,
    /// The Exec line, unexpanded.
    pub exec: String,
    /// Icon name or path.
    pub icon: Option<String>,
    /// The desktop file.
    pub path: PathBuf,
    /// Whether the program runs in a terminal.
    pub terminal: bool,
    /// Whether the program is a terminal itself (`TerminalEmulator` among its categories).
    pub emulator: bool,
    /// Working directory for the program.
    pub working_dir: Option<String>,
    /// Menu category the entry is listed under.
    pub category: String,
    /// The window class or application ID the entry's windows carry, when it differs from the
    /// desktop file ID.
    pub wm_class: Option<String>,
}

impl Entry {
    /// Whether the entry matches a lowercase search text on its name, generic name, keywords,
    /// or executable.
    pub fn matches(&self, needle: &str) -> bool {
        if needle.is_empty() {
            return true;
        }
        self.name.to_lowercase().contains(needle)
            || self
                .generic_name
                .as_ref()
                .is_some_and(|g| g.to_lowercase().contains(needle))
            || self.keywords.iter().any(|k| k.to_lowercase().contains(needle))
            || self.program().is_some_and(|f| f.to_lowercase().contains(needle))
    }

    /// How well the entry matches a lowercase search text, lower ranking first: a name that
    /// starts with the text, then a word of the name, then anything else.
    pub fn rank(&self, needle: &str) -> u8 {
        let name = self.name.to_lowercase();
        if name.starts_with(needle) {
            0
        } else if name.split_whitespace().any(|w| w.starts_with(needle)) {
            1
        } else if name.contains(needle) {
            2
        } else {
            3
        }
    }

    /// The file name of the program the Exec line runs.
    pub fn program(&self) -> Option<&str> {
        self.exec
            .split_whitespace()
            .next()
            .and_then(|e| Path::new(e).file_name())
            .and_then(|f| f.to_str())
    }
}

/// A menu category and the entries it lists, sorted by name.
#[derive(Clone, Debug, Default)]
pub struct Category {
    /// Display name.
    pub name: String,
    /// Indexes into the application list.
    pub entries: Vec<usize>,
}

/// Everything loaded from the desktop files.
#[derive(Default)]
pub struct Apps {
    /// Applications from the data directories, in load order.
    pub entries: Vec<Entry>,
    /// Categories sorted by name with the miscellaneous one last.
    pub categories: Vec<Category>,
    /// Launchers in the user's Desktop directory, sorted by name.
    pub desktop: Vec<Entry>,
    /// The applications directories and their modification times when they were read, so a
    /// reader can tell whether an entry was added or removed since.
    stamps: Vec<(PathBuf, Option<SystemTime>)>,
}

impl Apps {
    /// Scans the applications directories and the Desktop directory; `terminal_available` says
    /// whether entries that need a terminal can run.
    pub fn load(terminal_available: bool) -> Apps {
        let locales = locale_suffixes();
        let mut apps = Apps::default();
        let mut seen: HashSet<String> = HashSet::new();
        for dir in applications_dirs() {
            scan_dir(&dir, &locales, terminal_available, &mut seen, &mut apps.entries);
        }
        apps.stamps = directory_stamps();
        apps.build_categories();
        apps.desktop = load_desktop_dir(&locales);
        apps
    }

    /// Leave out the entry of a desktop file ID.
    pub fn without(&mut self, id: &str) {
        self.entries.retain(|e| e.id != id);
        self.build_categories();
    }

    /// Leave out every terminal and every entry that runs in one, for a session whose image
    /// keeps terminals out of its menus.
    pub fn without_terminals(&mut self) {
        self.entries.retain(|e| !e.emulator && !e.terminal);
        self.desktop.retain(|e| !e.emulator && !e.terminal);
        self.build_categories();
    }

    /// Whether an applications directory changed since the scan, so a list built from it is
    /// stale; a directory's modification time moves when a file is added, removed, or renamed
    /// in it.
    pub fn stale(&self) -> bool {
        self.stamps != directory_stamps()
    }

    /// Groups the entries into sorted categories.
    fn build_categories(&mut self) {
        let mut categories: Vec<Category> = Vec::new();
        for (index, entry) in self.entries.iter().enumerate() {
            match categories.iter_mut().find(|c| c.name == entry.category) {
                Some(category) => category.entries.push(index),
                None => categories.push(Category {
                    name: entry.category.clone(),
                    entries: vec![index],
                }),
            }
        }
        for category in &mut categories {
            category.entries.sort_by(|a, b| {
                self.entries[*a]
                    .name
                    .to_lowercase()
                    .cmp(&self.entries[*b].name.to_lowercase())
            });
        }
        categories.sort_by(|a, b| match (a.name == MISC, b.name == MISC) {
            (true, false) => std::cmp::Ordering::Greater,
            (false, true) => std::cmp::Ordering::Less,
            _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        });
        self.categories = categories;
    }

    /// Every application index sorted by name.
    pub fn all_sorted(&self) -> Vec<usize> {
        let mut all: Vec<usize> = (0..self.entries.len()).collect();
        all.sort_by(|a, b| {
            self.entries[*a]
                .name
                .to_lowercase()
                .cmp(&self.entries[*b].name.to_lowercase())
        });
        all
    }

    /// Finds an application by desktop file ID.
    pub fn find(&self, id: &str) -> Option<usize> {
        self.entries.iter().position(|e| e.id == id)
    }

    /// Applications matching a search as typed, best matches first.
    pub fn search(&self, query: &str) -> Vec<usize> {
        let needle = query.trim().to_lowercase();
        if needle.is_empty() {
            return Vec::new();
        }
        let mut hits: Vec<(u8, usize)> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.matches(&needle))
            .map(|(i, e)| (e.rank(&needle), i))
            .collect();
        hits.sort_by(|a, b| {
            a.0.cmp(&b.0).then_with(|| {
                self.entries[a.1]
                    .name
                    .to_lowercase()
                    .cmp(&self.entries[b.1].name.to_lowercase())
            })
        });
        hits.into_iter().map(|(_, i)| i).collect()
    }
}

/// The modification time of every applications directory, missing ones as `None`.
fn directory_stamps() -> Vec<(PathBuf, Option<SystemTime>)> {
    applications_dirs()
        .into_iter()
        .map(|dir| {
            let modified = std::fs::metadata(&dir).and_then(|m| m.modified()).ok();
            (dir, modified)
        })
        .collect()
}

/// The icon of the application whose windows carry an application ID or class: the entry
/// naming it as its window class, the entry with that desktop file ID, else the entry whose ID
/// is the longest prefix of it, the way a helper process such as `steamwebhelper` belongs to
/// `steam`.
pub fn icon_for_app_id(entries: &[Entry], app_id: &str) -> Option<String> {
    let lower = app_id.to_lowercase();
    if let Some(icon) = entries
        .iter()
        .find(|e| e.wm_class.as_ref().is_some_and(|c| c.to_lowercase() == lower))
        .and_then(|e| e.icon.clone())
    {
        return Some(icon);
    }
    if let Some(icon) = entries
        .iter()
        .find(|e| e.id.to_lowercase() == format!("{lower}.desktop"))
        .and_then(|e| e.icon.clone())
    {
        return Some(icon);
    }
    entries
        .iter()
        .filter_map(|e| {
            let stem = e.id.strip_suffix(".desktop").unwrap_or(&e.id).to_lowercase();
            (stem.len() >= 4 && lower.starts_with(&stem) && e.icon.is_some()).then_some((stem.len(), e))
        })
        .max_by_key(|(len, _)| *len)
        .and_then(|(_, e)| e.icon.clone())
}

/// The icon of the application a command line starts: the entry whose Exec line runs the same
/// program, by file name, else the one named like it. A menu item written without an icon
/// gets the one its application carries this way.
pub fn icon_for_command(entries: &[Entry], command: &str) -> Option<String> {
    let program = split_exec(command)
        .into_iter()
        .find(|arg| !arg.contains('=') && !arg.is_empty())?;
    let name = Path::new(&program).file_name()?.to_str()?.to_lowercase();
    if name.is_empty() || matches!(name.as_str(), "sh" | "bash" | "env") {
        return None;
    }
    entries
        .iter()
        .find(|e| e.icon.is_some() && e.program().is_some_and(|p| p.to_lowercase() == name))
        .and_then(|e| e.icon.clone())
        .or_else(|| icon_for_app_id(entries, &name))
}

/// The applications directories in precedence order: the user's data home, then each system
/// data directory.
pub fn applications_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![dirs::data_home().join("applications")];
    for dir in dirs::data_dirs() {
        dirs.push(dir.join("applications"));
    }
    dirs
}

/// The user's Desktop directory, from `user-dirs.dirs` where it names one.
pub fn desktop_dir() -> PathBuf {
    let user_dirs = dirs::home().join(".config/user-dirs.dirs");
    if let Ok(text) = std::fs::read_to_string(user_dirs) {
        for line in text.lines() {
            if let Some(value) = line.trim().strip_prefix("XDG_DESKTOP_DIR=") {
                let value = value.trim_matches('"');
                let path = value.replace("$HOME", &dirs::home().to_string_lossy());
                return PathBuf::from(path);
            }
        }
    }
    dirs::home().join("Desktop")
}

/// Locale suffixes for localized keys, most specific first: `ll_CC@mod`, `ll_CC`, `ll@mod`,
/// `ll`.
pub fn locale_suffixes() -> Vec<String> {
    let raw = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|var| std::env::var(var).ok().filter(|v| !v.is_empty()))
        .unwrap_or_default();
    let raw = raw.trim();
    if raw.is_empty() || raw == "C" || raw == "POSIX" {
        return Vec::new();
    }
    let (without_modifier, modifier) = match raw.split_once('@') {
        Some((base, m)) => (base, Some(m)),
        None => (raw, None),
    };
    let without_encoding = without_modifier
        .split_once('.')
        .map(|(b, _)| b)
        .unwrap_or(without_modifier);
    let (lang, country) = match without_encoding.split_once('_') {
        Some((l, c)) => (l, Some(c)),
        None => (without_encoding, None),
    };
    let mut suffixes = Vec::new();
    if let (Some(c), Some(m)) = (country, modifier) {
        suffixes.push(format!("{lang}_{c}@{m}"));
    }
    if let Some(c) = country {
        suffixes.push(format!("{lang}_{c}"));
    }
    if let Some(m) = modifier {
        suffixes.push(format!("{lang}@{m}"));
    }
    suffixes.push(lang.to_string());
    suffixes
}

/// The keys of one `[Desktop Entry]` group.
#[derive(Default)]
pub struct Group {
    pub keys: Vec<(String, String)>,
}

impl Group {
    /// The value of a plain key.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.keys.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// The best localized value of a key, falling back to the plain one.
    pub fn localized(&self, key: &str, locales: &[String]) -> Option<&str> {
        for suffix in locales {
            let wanted = format!("{key}[{suffix}]");
            if let Some(value) = self.get(&wanted) {
                return Some(value);
            }
        }
        self.get(key)
    }

    /// Whether a boolean key is set to true.
    pub fn flag(&self, key: &str) -> bool {
        self.get(key).is_some_and(|v| v.trim() == "true")
    }
}

/// Reads the `[Desktop Entry]` group of a file; other groups are ignored.
pub fn read_entry_group(path: &Path) -> Option<Group> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut group = Group::default();
    let mut in_entry = false;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with('[') {
            in_entry = line.trim() == "[Desktop Entry]";
            continue;
        }
        if !in_entry || line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            group
                .keys
                .push((key.trim().to_string(), value.trim().to_string()));
        }
    }
    if group.keys.is_empty() { None } else { Some(group) }
}

/// Builds an entry from a parsed group, or `None` when it is not a visible application; with
/// `strict` off only the entry type is checked, for launchers shown where they were placed.
pub fn build_entry(
    path: &Path,
    group: &Group,
    locales: &[String],
    terminal_available: bool,
    strict: bool,
) -> Option<Entry> {
    if strict {
        if group.get("Type").is_some_and(|t| t != "Application") {
            return None;
        }
        if group.flag("NoDisplay") || group.flag("Hidden") {
            return None;
        }
        if let Some(try_exec) = group.get("TryExec").filter(|v| !v.is_empty())
            && resolve_program(try_exec).is_none()
        {
            return None;
        }
    } else if group.get("Type").is_some_and(|t| t != "Application") {
        return None;
    }
    let name = group.localized("Name", locales)?.to_string();
    let exec = group.get("Exec")?.to_string();
    if name.is_empty() || exec.trim().is_empty() {
        return None;
    }
    let terminal = group.flag("Terminal");
    if strict {
        let program = exec_argv(&exec, None, "", path).into_iter().next()?;
        if program_disabled(&program) {
            return None;
        }
        if terminal && !terminal_available {
            return None;
        }
    }
    let keywords = group
        .localized("Keywords", locales)
        .map(|k| {
            k.split(';')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let emulator = group
        .get("Categories")
        .is_some_and(|cats| cats.split(';').any(|c| c.trim() == "TerminalEmulator"));
    let category = group
        .get("Categories")
        .and_then(|cats| {
            let listed: Vec<&str> = cats.split(';').map(|c| c.trim()).collect();
            CATEGORY_MAP
                .iter()
                .find(|(from, _)| listed.contains(from))
                .map(|(_, to)| to.to_string())
        })
        .unwrap_or_else(|| MISC.to_string());
    Some(Entry {
        id: path
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_default(),
        name,
        comment: group
            .localized("Comment", locales)
            .filter(|c| !c.is_empty())
            .map(str::to_string),
        generic_name: group
            .localized("GenericName", locales)
            .filter(|c| !c.is_empty())
            .map(str::to_string),
        keywords,
        exec,
        icon: group.get("Icon").filter(|i| !i.is_empty()).map(str::to_string),
        path: path.to_path_buf(),
        terminal,
        emulator,
        working_dir: group.get("Path").filter(|p| !p.is_empty()).map(str::to_string),
        category,
        wm_class: group
            .get("StartupWMClass")
            .filter(|c| !c.is_empty())
            .map(str::to_string),
    })
}

/// Scans one applications directory, skipping the desktop file IDs `seen` already holds.
fn scan_dir(
    dir: &Path,
    locales: &[String],
    terminal_available: bool,
    seen: &mut HashSet<String>,
    out: &mut Vec<Entry>,
) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<PathBuf> = read
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "desktop") && p.is_file())
        .collect();
    files.sort();
    for path in files {
        let id = path
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_default();
        if !seen.insert(id) {
            continue;
        }
        let Some(group) = read_entry_group(&path) else {
            continue;
        };
        if let Some(entry) = build_entry(&path, &group, locales, terminal_available, true) {
            out.push(entry);
        }
    }
}

/// Reads the launchers on the user's desktop; these are shown as placed, so only the entry type
/// is checked.
fn load_desktop_dir(locales: &[String]) -> Vec<Entry> {
    let dir = desktop_dir();
    let Ok(read) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for path in read.flatten().map(|e| e.path()) {
        if path.extension().is_none_or(|e| e != "desktop") || !path.is_file() {
            continue;
        }
        let Some(group) = read_entry_group(&path) else {
            continue;
        };
        if let Some(entry) = build_entry(&path, &group, locales, true, false) {
            entries.push(entry);
        }
    }
    entries.sort_by_key(|entry| entry.name.to_lowercase());
    entries
}

/// Reads one desktop entry by its ID from the applications directories, whether or not it is
/// shown in menus.
pub fn load_entry(id: &str) -> Option<Entry> {
    let locales = locale_suffixes();
    applications_dirs()
        .into_iter()
        .map(|dir| dir.join(id))
        .find(|path| path.is_file())
        .and_then(|path| {
            let group = read_entry_group(&path)?;
            build_entry(&path, &group, &locales, true, false)
        })
}

/// The directories searched for programs: PATH plus the user's local bin, which login shells
/// add but the session may not have.
fn search_path() -> Vec<PathBuf> {
    let raw = std::env::var("PATH").ok().filter(|p| !p.is_empty());
    let raw = raw.unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".to_string());
    let mut dirs: Vec<PathBuf> = raw
        .split(':')
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .collect();
    let local = dirs::home().join(".local/bin");
    if !dirs.contains(&local) {
        dirs.push(local);
    }
    dirs
}

/// Whether a file exists and may be executed by this user.
fn is_executable(path: &Path) -> bool {
    let Ok(c_path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `access` reads the NUL-terminated path alone and touches no other memory.
    unsafe { libc::access(c_path.as_ptr(), libc::X_OK) == 0 }
}

/// Finds a program as an absolute path or on the search path: the executable's path, or `None`
/// when none can be run.
pub fn resolve_program(program: &str) -> Option<PathBuf> {
    let program = program.trim();
    if program.is_empty() {
        return None;
    }
    if program.contains('/') {
        let path = PathBuf::from(program);
        return is_executable(&path).then_some(path);
    }
    search_path()
        .into_iter()
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
}

/// Whether a program is installed but may not be run, or is an absolute path that does not
/// exist. A bare name found nowhere is not disabled, since it may be provided by a wrapper the
/// session cannot see.
fn program_disabled(program: &str) -> bool {
    if program.contains('/') {
        return !is_executable(Path::new(program));
    }
    let mut present = false;
    for dir in search_path() {
        let candidate = dir.join(program);
        if candidate.is_file() {
            present = true;
            if is_executable(&candidate) {
                return false;
            }
        }
    }
    present
}

/// The terminal command to wrap entries in, as argv without the program to run: `configured`
/// (a setting, else the `TERMINAL` variable) where it names a program that runs, else the first
/// of the known terminals installed. Under Wayland foot is tried before the distribution's
/// alternatives link, which points at an X terminal; foot cannot run without Wayland, so
/// elsewhere the link keeps its say.
pub fn find_terminal(configured: Option<&str>) -> Option<Vec<String>> {
    let configured = configured
        .map(str::to_string)
        .or_else(|| std::env::var("TERMINAL").ok())
        .filter(|t| !t.is_empty());
    if let Some(command) = configured {
        let mut argv = split_exec(&command);
        if let Some(program) = argv.first()
            && resolve_program(program).is_some()
        {
            if argv.len() == 1 {
                argv.push("-e".to_string());
            }
            return Some(argv);
        }
    }
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some_and(|d| !d.is_empty());
    let preferred = if wayland { Some("foot") } else { None };
    preferred
        .into_iter()
        .chain(TERMINALS.iter().copied())
        .find(|t| resolve_program(t).is_some())
        .map(|t| vec![t.to_string(), "-e".to_string()])
}

/// Splits an Exec value into arguments following the desktop entry quoting rules: spaces
/// separate, double quotes group, and inside quotes a backslash escapes the next character.
pub fn split_exec(exec: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut has_arg = false;
    let mut chars = exec.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                has_arg = true;
            }
            '\\' if in_quotes => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            ' ' | '\t' if !in_quotes => {
                if has_arg || !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                    has_arg = false;
                }
            }
            _ => current.push(c),
        }
    }
    if has_arg || !current.is_empty() {
        args.push(current);
    }
    args
}

/// Expands the field codes of an Exec value for a launch without files: file and URL codes
/// expand to nothing, `%i` to the icon option, `%c` to the name, and `%k` to the desktop file
/// path.
pub fn exec_argv(exec: &str, icon: Option<&str>, name: &str, path: &Path) -> Vec<String> {
    exec_argv_files(exec, icon, name, path, &[])
}

/// Expands the field codes of an Exec value with files to open: `%f` and `%u` take the first
/// file, `%F` and `%U` every file, as paths or `file://` URIs; the rest expand as [`exec_argv`]
/// does. When the Exec line has no file code the files are appended.
pub fn exec_argv_files(
    exec: &str,
    icon: Option<&str>,
    name: &str,
    path: &Path,
    files: &[PathBuf],
) -> Vec<String> {
    let mut argv = Vec::new();
    let mut placed = files.is_empty();
    for arg in split_exec(exec) {
        if arg == "%i" {
            if let Some(icon) = icon {
                argv.push("--icon".to_string());
                argv.push(icon.to_string());
            }
            continue;
        }
        if matches!(arg.as_str(), "%f" | "%F" | "%u" | "%U") {
            if files.is_empty() {
                continue;
            }
            placed = true;
            let uri = arg == "%u" || arg == "%U";
            let count = if arg == "%f" || arg == "%u" {
                1
            } else {
                files.len()
            };
            for file in files.iter().take(count) {
                argv.push(if uri {
                    file_uri(file)
                } else {
                    file.to_string_lossy().to_string()
                });
            }
            continue;
        }
        let mut expanded = String::new();
        let mut chars = arg.chars();
        while let Some(c) = chars.next() {
            if c != '%' {
                expanded.push(c);
                continue;
            }
            match chars.next() {
                Some('%') => expanded.push('%'),
                Some('c') => expanded.push_str(name),
                Some('k') => expanded.push_str(&path.to_string_lossy()),
                Some(_) | None => {}
            }
        }
        let was_only_code = arg.len() == 2 && arg.starts_with('%') && expanded.is_empty();
        if was_only_code {
            continue;
        }
        if !expanded.is_empty() || !arg.contains('%') {
            argv.push(expanded);
        }
    }
    if !placed && !argv.is_empty() {
        argv.extend(files.iter().map(|f| f.to_string_lossy().to_string()));
    }
    argv
}

/// A `file://` URI for a path, every byte outside the unreserved set percent-encoded.
pub fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for byte in path.as_os_str().as_bytes() {
        let c = *byte as char;
        if c.is_ascii_alphanumeric() || "/-._~".contains(c) {
            uri.push(c);
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

/// An argv as one shell command line, each argument single-quoted where it needs to be, for a
/// launcher that runs commands through `sh -c`.
pub fn shell_command(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| {
            if !arg.is_empty()
                && arg
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_./=:,+%@".contains(c))
            {
                arg.clone()
            } else {
                format!("'{}'", arg.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Starts a program fully detached from the caller: forked twice so it is reparented to init
/// and never becomes a zombie, in its own session, with its standard streams on `/dev/null`.
pub fn spawn_detached(argv: &[String], working_dir: Option<&str>) {
    let c_args: Vec<CString> = argv
        .iter()
        .filter_map(|a| CString::new(a.as_str()).ok())
        .collect();
    if c_args.is_empty() {
        return;
    }
    let mut ptrs: Vec<*const libc::c_char> = c_args.iter().map(|a| a.as_ptr()).collect();
    ptrs.push(std::ptr::null());
    let dir = working_dir.and_then(|d| CString::new(d).ok());
    let null = CString::new("/dev/null").expect("static string");
    // SAFETY: the child calls only async-signal-safe functions on memory the parent prepared
    // before the fork, and the parent waits for the intermediate child it created.
    unsafe {
        let pid = libc::fork();
        if pid != 0 {
            if pid > 0 {
                let mut status = 0;
                libc::waitpid(pid, &mut status, 0);
            }
            return;
        }
        libc::setsid();
        if libc::fork() != 0 {
            libc::_exit(0);
        }
        let fd = libc::open(null.as_ptr(), libc::O_RDWR);
        if fd >= 0 {
            libc::dup2(fd, 0);
            libc::dup2(fd, 1);
            libc::dup2(fd, 2);
            if fd > 2 {
                libc::close(fd);
            }
        }
        if let Some(dir) = dir {
            libc::chdir(dir.as_ptr());
        }
        libc::execvp(ptrs[0], ptrs.as_ptr());
        libc::_exit(127);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, exec: &str, icon: &str, class: Option<&str>) -> Entry {
        Entry {
            id: id.to_string(),
            name: id.to_string(),
            comment: None,
            generic_name: None,
            keywords: Vec::new(),
            exec: exec.to_string(),
            icon: Some(icon.to_string()),
            path: PathBuf::from("/tmp"),
            terminal: false,
            emulator: false,
            working_dir: None,
            category: MISC.to_string(),
            wm_class: class.map(str::to_string),
        }
    }

    /// An Exec value splits on its quoting rules and its field codes expand or drop out.
    #[test]
    fn exec_lines_split_and_expand() {
        let argv = split_exec(r#"/bin/sh -c "$HOME/.local/bin/proot-apps run x %U" extra"#);
        assert_eq!(
            argv,
            vec!["/bin/sh", "-c", "$HOME/.local/bin/proot-apps run x %U", "extra"]
        );
        assert_eq!(
            split_exec(r#"prog "a \"quoted\" arg""#),
            vec!["prog", "a \"quoted\" arg"]
        );
        assert_eq!(split_exec("prog  \"\""), vec!["prog", ""]);
        let path = Path::new("/usr/share/applications/app.desktop");
        let argv = exec_argv("app %U --name=%c %i tail %%", Some("icon"), "My App", path);
        assert_eq!(argv, vec!["app", "--name=My App", "--icon", "icon", "tail", "%"]);
        assert_eq!(exec_argv("%F", None, "", path), Vec::<String>::new());
        assert_eq!(
            exec_argv("app %k", None, "", path),
            vec!["app".to_string(), path.to_string_lossy().to_string()]
        );
        let files = vec![PathBuf::from("/tmp/a b.txt"), PathBuf::from("/tmp/c.txt")];
        assert_eq!(
            exec_argv_files("app %F", None, "", path, &files),
            vec!["app", "/tmp/a b.txt", "/tmp/c.txt"]
        );
        assert_eq!(
            exec_argv_files("app %u", None, "", path, &files),
            vec!["app", "file:///tmp/a%20b.txt"]
        );
        assert_eq!(
            exec_argv_files("app --flag", None, "", path, &files[..1]),
            vec!["app", "--flag", "/tmp/a b.txt"]
        );
        assert_eq!(
            shell_command(&["app".to_string(), "--name=My App".to_string(), "it's".to_string()]),
            "app '--name=My App' 'it'\\''s'"
        );
    }

    /// The first listed category of the map wins, a localized name is taken, and hidden entries
    /// are left out of the strict scan alone.
    #[test]
    fn entries_take_their_category_name_and_visibility() {
        let group = Group {
            keys: vec![
                ("Type".into(), "Application".into()),
                ("Name".into(), "Thing".into()),
                ("Name[de]".into(), "Ding".into()),
                ("Exec".into(), "/bin/true".into()),
                ("Categories".into(), "Qt;Network;WebBrowser;".into()),
            ],
        };
        let entry = build_entry(
            Path::new("/tmp/thing.desktop"),
            &group,
            &["de".to_string()],
            true,
            true,
        )
        .unwrap();
        assert_eq!(entry.category, "Internet");
        assert_eq!(entry.name, "Ding");
        assert_eq!(entry.id, "thing.desktop");
        assert!(entry.matches("din"));
        assert!(entry.matches("true"));
        let hidden = Group {
            keys: vec![
                ("Type".into(), "Application".into()),
                ("Name".into(), "Hidden".into()),
                ("Exec".into(), "/bin/true".into()),
                ("NoDisplay".into(), "true".into()),
            ],
        };
        assert!(build_entry(Path::new("/tmp/h.desktop"), &hidden, &[], true, true).is_none());
        assert!(build_entry(Path::new("/tmp/h.desktop"), &hidden, &[], true, false).is_some());
    }

    /// An application's icon is found by its window class, its desktop file ID, the longest ID
    /// prefix of a helper's class, and by the program a command line runs.
    #[test]
    fn icons_are_found_by_class_id_prefix_and_command() {
        let entries = vec![
            entry("steam.desktop", "/usr/bin/steam %U", "steam", None),
            entry(
                "org.mozilla.firefox.desktop",
                "firefox %u",
                "firefox",
                Some("Firefox"),
            ),
            entry("st.desktop", "st", "st", None),
            entry("mousepad.desktop", "mousepad %F", "org.xfce.mousepad", None),
        ];
        assert_eq!(icon_for_app_id(&entries, "Firefox").as_deref(), Some("firefox"));
        assert_eq!(icon_for_app_id(&entries, "steam").as_deref(), Some("steam"));
        assert_eq!(
            icon_for_app_id(&entries, "steamwebhelper").as_deref(),
            Some("steam")
        );
        assert_eq!(icon_for_app_id(&entries, "stterm"), None);
        assert_eq!(
            icon_for_command(&entries, "mousepad").as_deref(),
            Some("org.xfce.mousepad")
        );
        assert_eq!(
            icon_for_command(&entries, "FOO=1 /opt/firefox/firefox --new-window").as_deref(),
            Some("firefox")
        );
        assert_eq!(icon_for_command(&entries, "sh -c 'steam'"), None);
        assert_eq!(icon_for_command(&entries, "x-terminal-emulator"), None);
    }
}
