//! The shared mime database as the freedesktop specification lays it out:
//! file name globs, the generic icon of each type, the type comments, and
//! which desktop entries open a type, read from the `mimeapps.list` files
//! and the `mimeinfo.cache` of every applications directory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::config;

/// The type of a folder.
pub const DIRECTORY: &str = "inode/directory";
/// The type of a file nothing else matches.
pub const UNKNOWN: &str = "application/octet-stream";

/// One glob of the database.
#[derive(Clone, Debug)]
struct Glob {
    weight: u32,
    mime: String,
    pattern: String,
    case_sensitive: bool,
}

/// The loaded database.
#[derive(Default)]
pub struct MimeDb {
    /// Literal `*.ext` globs by lowercase extension, the heaviest first.
    extensions: HashMap<String, Vec<(u32, String)>>,
    /// Globs that are not a plain extension, heaviest first.
    patterns: Vec<Glob>,
    /// Generic icon names by type.
    generic_icons: HashMap<String, String>,
    /// Parent types by type.
    subclasses: HashMap<String, Vec<String>>,
    /// Comments read so far, by type.
    comments: HashMap<String, Option<String>>,
    /// Desktop file IDs that open a type, the default first.
    handlers: HashMap<String, Vec<String>>,
    /// Whether the handler lists were read.
    handlers_loaded: bool,
}

impl MimeDb {
    /// Reads the database from the user's and the system's mime directories.
    pub fn load() -> MimeDb {
        let mut db = MimeDb::default();
        for dir in mime_dirs().iter().rev() {
            if let Ok(text) = std::fs::read_to_string(dir.join("globs2")) {
                db.read_globs(&text);
            }
            if let Ok(text) = std::fs::read_to_string(dir.join("generic-icons")) {
                for line in text.lines() {
                    if let Some((mime, icon)) = line.split_once(':') {
                        db.generic_icons
                            .insert(mime.trim().to_string(), icon.trim().to_string());
                    }
                }
            }
            if let Ok(text) = std::fs::read_to_string(dir.join("subclasses")) {
                for line in text.lines() {
                    if let Some((child, parent)) = line.split_once(' ') {
                        db.subclasses
                            .entry(child.trim().to_string())
                            .or_default()
                            .push(parent.trim().to_string());
                    }
                }
            }
        }
        for list in db.extensions.values_mut() {
            list.sort_by_key(|(weight, _)| std::cmp::Reverse(*weight));
        }
        db.patterns
            .sort_by_key(|pattern| std::cmp::Reverse(pattern.weight));
        db
    }

    /// Adds the globs of one `globs2` file.
    fn read_globs(&mut self, text: &str) {
        for line in text.lines() {
            if line.starts_with('#') {
                continue;
            }
            let mut parts = line.split(':');
            let (Some(weight), Some(mime), Some(pattern)) = (parts.next(), parts.next(), parts.next()) else {
                continue;
            };
            let weight: u32 = weight.trim().parse().unwrap_or(50);
            let flags = parts.next().unwrap_or("");
            let case_sensitive = flags.contains("cs");
            if let Some(ext) = pattern.strip_prefix("*.")
                && !ext.contains(['*', '?', '['])
                && !case_sensitive
            {
                self.extensions
                    .entry(ext.to_ascii_lowercase())
                    .or_default()
                    .push((weight, mime.to_string()));
            } else {
                self.patterns.push(Glob {
                    weight,
                    mime: mime.to_string(),
                    pattern: pattern.to_string(),
                    case_sensitive,
                });
            }
        }
    }

    /// The type of a file by its name.
    ///
    /// # Arguments
    /// * `name` - The file name.
    /// * `directory` - Whether the file is a directory.
    pub fn type_of(&self, name: &str, directory: bool) -> String {
        if directory {
            return DIRECTORY.to_string();
        }
        let lower = name.to_ascii_lowercase();
        let mut best: Option<(u32, &str)> = None;
        let mut rest = lower.as_str();
        while let Some(dot) = rest.find('.') {
            rest = &rest[dot + 1..];
            if let Some(list) = self.extensions.get(rest)
                && let Some((weight, mime)) = list.first()
                && best.is_none_or(|(w, _)| *weight > w)
            {
                best = Some((*weight, mime));
            }
        }
        for glob in &self.patterns {
            if best.is_some_and(|(w, _)| w >= glob.weight) {
                break;
            }
            let matched = if glob.case_sensitive {
                glob_matches(&glob.pattern, name)
            } else {
                glob_matches(&glob.pattern.to_ascii_lowercase(), &lower)
            };
            if matched {
                best = Some((glob.weight, &glob.mime));
            }
        }
        best.map_or_else(|| UNKNOWN.to_string(), |(_, mime)| mime.to_string())
    }

    /// Icon names for a type, the most specific first.
    ///
    /// # Arguments
    /// * `mime` - The type.
    pub fn icon_names(&self, mime: &str) -> Vec<String> {
        let mut names = vec![mime.replace('/', "-")];
        if let Some(generic) = self.generic_icons.get(mime) {
            names.push(generic.clone());
        }
        if let Some(parents) = self.subclasses.get(mime) {
            for parent in parents {
                names.push(parent.replace('/', "-"));
                if let Some(generic) = self.generic_icons.get(parent) {
                    names.push(generic.clone());
                }
            }
        }
        if let Some((media, _)) = mime.split_once('/') {
            names.push(format!("{media}-x-generic"));
        }
        if mime == DIRECTORY {
            names.push("folder".to_string());
        } else if mime.starts_with("text/") {
            names.push("text-x-generic".to_string());
        } else {
            names.push("application-x-generic".to_string());
        }
        names.dedup();
        names
    }

    /// The human readable comment of a type, from its XML file.
    ///
    /// # Arguments
    /// * `mime` - The type.
    pub fn comment(&mut self, mime: &str) -> Option<String> {
        if let Some(known) = self.comments.get(mime) {
            return known.clone();
        }
        let comment = mime_dirs()
            .iter()
            .map(|dir| dir.join(format!("{mime}.xml")))
            .find(|p| p.is_file())
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|text| {
                let start = text.find("<comment>")? + "<comment>".len();
                let end = text[start..].find("</comment>")? + start;
                Some(text[start..end].trim().to_string())
            });
        self.comments.insert(mime.to_string(), comment.clone());
        comment
    }

    /// The desktop file IDs that open a type, the default first, then the
    /// user's additions, then every entry whose cache lists the type.  Parent
    /// types supply handlers after the type's own.
    ///
    /// # Arguments
    /// * `mime` - The type.
    pub fn handlers(&mut self, mime: &str) -> Vec<String> {
        if !self.handlers_loaded {
            self.load_handlers();
        }
        let mut ids: Vec<String> = self.handlers.get(mime).cloned().unwrap_or_default();
        if let Some(parents) = self.subclasses.get(mime).cloned() {
            for parent in parents {
                for id in self.handlers.get(&parent).cloned().unwrap_or_default() {
                    if !ids.contains(&id) {
                        ids.push(id);
                    }
                }
            }
        }
        ids
    }

    /// Reads the association files.
    fn load_handlers(&mut self) {
        self.handlers_loaded = true;
        let mut defaults: HashMap<String, Vec<String>> = HashMap::new();
        let mut added: HashMap<String, Vec<String>> = HashMap::new();
        let mut removed: HashMap<String, Vec<String>> = HashMap::new();
        for path in mimeapps_files() {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            read_mimeapps(&text, &mut defaults, &mut added, &mut removed);
        }
        let mut cached: HashMap<String, Vec<String>> = HashMap::new();
        for dir in crate::entries::applications_dirs() {
            if let Ok(text) = std::fs::read_to_string(dir.join("mimeinfo.cache")) {
                read_cache(&text, &mut cached);
            }
        }
        let mut types: Vec<String> = defaults.keys().cloned().collect();
        types.extend(added.keys().cloned());
        types.extend(cached.keys().cloned());
        types.sort();
        types.dedup();
        for mime in types {
            let mut list: Vec<String> = Vec::new();
            for source in [&defaults, &added, &cached] {
                for id in source.get(&mime).into_iter().flatten() {
                    if !list.contains(id) && !removed.get(&mime).is_some_and(|r| r.contains(id)) {
                        list.push(id.clone());
                    }
                }
            }
            self.handlers.insert(mime, list);
        }
    }
}

/// Whether a shell style glob matches a name; `*`, `?` and `[...]` are understood.
///
/// # Arguments
/// * `pattern` - The glob.
/// * `name` - The file name.
pub fn glob_matches(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    fn go(p: &[char], n: &[char]) -> bool {
        match p.first() {
            None => n.is_empty(),
            Some('*') => (0..=n.len()).any(|i| go(&p[1..], &n[i..])),
            Some('?') => !n.is_empty() && go(&p[1..], &n[1..]),
            Some('[') => {
                let Some(close) = p.iter().position(|c| *c == ']') else {
                    return !n.is_empty() && n[0] == '[' && go(&p[1..], &n[1..]);
                };
                let Some(first) = n.first() else {
                    return false;
                };
                let set = &p[1..close];
                let (negate, set) = match set.first() {
                    Some('!') | Some('^') => (true, &set[1..]),
                    _ => (false, set),
                };
                let mut matched = false;
                let mut i = 0;
                while i < set.len() {
                    if i + 2 < set.len() && set[i + 1] == '-' {
                        if (set[i]..=set[i + 2]).contains(first) {
                            matched = true;
                        }
                        i += 3;
                    } else {
                        if set[i] == *first {
                            matched = true;
                        }
                        i += 1;
                    }
                }
                matched != negate && go(&p[close + 1..], &n[1..])
            }
            Some(c) => !n.is_empty() && n[0] == *c && go(&p[1..], &n[1..]),
        }
    }
    go(&p, &n)
}

/// The mime directories, the user's first.
fn mime_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![config::data_home().join("mime")];
    for dir in config::data_dirs() {
        dirs.push(dir.join("mime"));
    }
    dirs
}

/// The `mimeapps.list` files in precedence order.
fn mimeapps_files() -> Vec<PathBuf> {
    let mut files = vec![user_mimeapps()];
    let config_dirs = std::env::var("XDG_CONFIG_DIRS")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "/etc/xdg".to_string());
    for dir in config_dirs.split(':').filter(|d| !d.is_empty()) {
        files.push(Path::new(dir).join("mimeapps.list"));
    }
    for dir in crate::entries::applications_dirs() {
        files.push(dir.join("mimeapps.list"));
    }
    files
}

/// The user's own `mimeapps.list`.
pub fn user_mimeapps() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| config::home().join(".config"))
        .join("mimeapps.list")
}

/// Reads the three groups of a `mimeapps.list` into the maps, keeping earlier
/// files' entries ahead of later ones.
fn read_mimeapps(
    text: &str,
    defaults: &mut HashMap<String, Vec<String>>,
    added: &mut HashMap<String, Vec<String>>,
    removed: &mut HashMap<String, Vec<String>>,
) {
    let mut group = "";
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            group = line;
            continue;
        }
        let Some((mime, ids)) = line.split_once('=') else {
            continue;
        };
        let target = match group {
            "[Default Applications]" => &mut *defaults,
            "[Added Associations]" => &mut *added,
            "[Removed Associations]" => &mut *removed,
            _ => continue,
        };
        let list = target.entry(mime.trim().to_string()).or_default();
        for id in ids.split(';').map(str::trim).filter(|id| !id.is_empty()) {
            if !list.iter().any(|existing| existing == id) {
                list.push(id.to_string());
            }
        }
    }
}

/// Reads a `mimeinfo.cache` into the map.
fn read_cache(text: &str, cached: &mut HashMap<String, Vec<String>>) {
    for line in text.lines() {
        let Some((mime, ids)) = line.split_once('=') else {
            continue;
        };
        let list = cached.entry(mime.trim().to_string()).or_default();
        for id in ids.split(';').map(str::trim).filter(|id| !id.is_empty()) {
            if !list.iter().any(|existing| existing == id) {
                list.push(id.to_string());
            }
        }
    }
}

/// Whether a type is an image the desktop can decode for thumbnails.
///
/// # Arguments
/// * `mime` - The type.
pub fn is_thumbnail_image(mime: &str) -> bool {
    matches!(mime, "image/png" | "image/jpeg")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> MimeDb {
        let mut db = MimeDb::default();
        db.read_globs(
            "80:text/html:*.html\n50:image/png:*.png\n50:text/x-makefile:Makefile:cs\n60:text/x-python3:*.py3\n50:application/x-sharedlib:*.so.[0-9]*\n50:text/x-readme:README*\n",
        );
        db.generic_icons
            .insert("text/html".to_string(), "text-html".to_string());
        db.subclasses
            .insert("text/x-python3".to_string(), vec!["text/plain".to_string()]);
        db
    }

    #[test]
    fn matches_extensions_and_patterns() {
        let db = db();
        assert_eq!(db.type_of("Index.HTML", false), "text/html");
        assert_eq!(db.type_of("a.tar.png", false), "image/png");
        assert_eq!(db.type_of("Makefile", false), "text/x-makefile");
        assert_eq!(db.type_of("makefile", false), UNKNOWN);
        assert_eq!(db.type_of("libc.so.6", false), "application/x-sharedlib");
        assert_eq!(db.type_of("README.txt", false), "text/x-readme");
        assert_eq!(db.type_of("anything", true), DIRECTORY);
    }

    #[test]
    fn icon_names_fall_back() {
        let db = db();
        assert_eq!(
            db.icon_names("text/x-python3"),
            vec!["text-x-python3", "text-plain", "text-x-generic"]
        );
        assert_eq!(db.icon_names("text/html"), vec!["text-html", "text-x-generic"]);
        assert_eq!(
            db.icon_names("video/mp4"),
            vec!["video-mp4", "video-x-generic", "application-x-generic"]
        );
        assert_eq!(
            db.icon_names(DIRECTORY),
            vec!["inode-directory", "inode-x-generic", "folder"]
        );
    }

    #[test]
    fn globs() {
        assert!(glob_matches("*.tar.[gx]z", "a.tar.xz"));
        assert!(!glob_matches("*.tar.[gx]z", "a.tar.bz"));
        assert!(glob_matches("[!.]*", "visible"));
        assert!(!glob_matches("[!.]*", ".hidden"));
        assert!(glob_matches("?.c", "a.c"));
    }

    #[test]
    fn mimeapps_precedence() {
        let mut defaults = HashMap::new();
        let mut added = HashMap::new();
        let mut removed = HashMap::new();
        read_mimeapps(
            "[Default Applications]\nimage/png=viewer.desktop;\n[Added Associations]\nimage/png=gimp.desktop;viewer.desktop;\n[Removed Associations]\nimage/png=bad.desktop;\n",
            &mut defaults,
            &mut added,
            &mut removed,
        );
        assert_eq!(defaults["image/png"], vec!["viewer.desktop"]);
        assert_eq!(added["image/png"], vec!["gimp.desktop", "viewer.desktop"]);
        assert_eq!(removed["image/png"], vec!["bad.desktop"]);
    }
}
