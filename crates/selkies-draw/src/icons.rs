/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Icon lookup following the XDG icon theme specification: the selected theme,
//! the themes it inherits, hicolor, then the pixmaps directories and finally
//! any installed theme, since applications often drop their icon into one of
//! their own.  Loaded icons are cached by name and pixel size.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tiny_skia::Pixmap;

use crate::dirs;
use crate::render;
use crate::xpm;

/// Themes tried in order when none is configured; hicolor is always appended.
const DEFAULT_THEMES: [&str; 5] = ["Papirus", "breeze", "Adwaita", "Yaru", "elementary"];

/// File extensions an icon may carry, in the order they are preferred.
const EXTENSIONS: [&str; 3] = ["svg", "png", "xpm"];

/// Icons stood in for by another when a theme lacks them.
const STAND_INS: [(&str, &str); 3] = [
    ("system-file-manager", "folder"),
    ("drive-harddisk", "folder"),
    ("folder-documents", "folder"),
];

/// How a theme subdirectory matches sizes.
#[derive(Clone, Copy, Debug)]
enum DirType {
    Fixed,
    Scalable { min: u32, max: u32 },
    Threshold(u32),
}

/// One subdirectory of a theme as its `index.theme` describes it.
#[derive(Clone, Debug)]
struct SubDir {
    path: String,
    size: u32,
    scale: u32,
    kind: DirType,
}

impl SubDir {
    /// Whether the directory holds icons of exactly this physical size.
    fn matches(&self, size: u32) -> bool {
        let logical = size.div_ceil(self.scale);
        match self.kind {
            DirType::Fixed => self.size == logical,
            DirType::Scalable { min, max } => (min..=max).contains(&logical),
            DirType::Threshold(t) => logical.abs_diff(self.size) <= t,
        }
    }

    /// How far the directory's icons are from a physical size.
    fn distance(&self, size: u32) -> u32 {
        let physical = self.size * self.scale;
        match self.kind {
            DirType::Fixed => physical.abs_diff(size),
            DirType::Scalable { min, max } => {
                let low = min * self.scale;
                let high = max * self.scale;
                low.saturating_sub(size).max(size.saturating_sub(high))
            }
            DirType::Threshold(t) => {
                let low = self.size.saturating_sub(t) * self.scale;
                let high = (self.size + t) * self.scale;
                low.saturating_sub(size).max(size.saturating_sub(high))
            }
        }
    }
}

/// A theme located in one or more search roots.
#[derive(Debug)]
struct Theme {
    roots: Vec<PathBuf>,
    dirs: Vec<SubDir>,
}

/// The icon lookup and cache.
pub struct Icons {
    search_roots: Vec<PathBuf>,
    chain: Vec<Theme>,
    pixmap_dirs: Vec<PathBuf>,
    cache: HashMap<(String, u32), Option<Arc<Pixmap>>>,
    other_themes: Option<Vec<PathBuf>>,
}

impl Icons {
    /// Builds the theme chain for a theme name, or a detected one when `None`.
    ///
    /// # Arguments
    /// * `theme` - The configured theme name, if any.
    pub fn new(theme: Option<&str>) -> Icons {
        let mut search_roots = vec![dirs::home().join(".icons"), dirs::data_home().join("icons")];
        for dir in dirs::data_dirs() {
            search_roots.push(dir.join("icons"));
        }
        search_roots.push(PathBuf::from("/usr/share/pixmaps"));
        let pixmap_dirs: Vec<PathBuf> = dirs::data_dirs()
            .iter()
            .map(|d| d.join("pixmaps"))
            .chain(std::iter::once(dirs::data_home().join("pixmaps")))
            .collect();

        let mut icons = Icons {
            search_roots,
            chain: Vec::new(),
            pixmap_dirs,
            cache: HashMap::new(),
            other_themes: None,
        };
        let selected = theme.map(|t| t.to_string()).or_else(gtk_icon_theme).or_else(|| {
            DEFAULT_THEMES
                .iter()
                .find(|t| icons.theme_exists(t))
                .map(|t| t.to_string())
        });
        let mut queue: Vec<String> = selected.into_iter().collect();
        let mut seen: Vec<String> = Vec::new();
        while let Some(name) = queue.first().cloned() {
            queue.remove(0);
            if seen.contains(&name) {
                continue;
            }
            seen.push(name.clone());
            if let Some((theme, inherits)) = icons.load_theme(&name) {
                icons.chain.push(theme);
                queue.extend(inherits);
            }
        }
        if !seen.iter().any(|n| n == "hicolor")
            && let Some((theme, _)) = icons.load_theme("hicolor")
        {
            icons.chain.push(theme);
        }
        icons
    }

    /// Whether a theme directory exists in any search root.
    fn theme_exists(&self, name: &str) -> bool {
        self.search_roots
            .iter()
            .any(|root| root.join(name).join("index.theme").is_file())
    }

    /// Reads a theme's `index.theme` from every root that has it.
    fn load_theme(&self, name: &str) -> Option<(Theme, Vec<String>)> {
        let roots: Vec<PathBuf> = self
            .search_roots
            .iter()
            .map(|root| root.join(name))
            .filter(|dir| dir.is_dir())
            .collect();
        if roots.is_empty() {
            return None;
        }
        let index = roots
            .iter()
            .map(|r| r.join("index.theme"))
            .find(|p| p.is_file())?;
        let text = std::fs::read_to_string(index).ok()?;
        let (dirs, inherits) = parse_index(&text);
        Some((Theme { roots, dirs }, inherits))
    }

    /// Finds and loads an icon at a physical pixel size.
    ///
    /// # Arguments
    /// * `name` - An icon name, with or without extension, or an absolute path.
    /// * `size` - Physical pixel size of the longer side.
    ///
    /// # Returns
    /// The icon scaled to the size, or `None` when nothing matched.
    pub fn get(&mut self, name: &str, size: u32) -> Option<Arc<Pixmap>> {
        let name = name.trim();
        if name.is_empty() || size == 0 {
            return None;
        }
        let key = (name.to_string(), size);
        if let Some(cached) = self.cache.get(&key) {
            return cached.clone();
        }
        let found = self.lookup(name, size).map(Arc::new);
        self.cache.insert(key, found.clone());
        found
    }

    /// The uncached lookup behind `get`.
    fn lookup(&mut self, name: &str, size: u32) -> Option<Pixmap> {
        let path = self.locate(name, size)?;
        load_file(&path, size)
    }

    /// Finds the file an icon name resolves to for a size, without loading it.
    ///
    /// # Arguments
    /// * `name` - An icon name, with or without extension, or an absolute path.
    /// * `size` - Physical pixel size wanted.
    pub fn locate(&mut self, name: &str, size: u32) -> Option<PathBuf> {
        if name.starts_with('/') {
            let path = PathBuf::from(name);
            if path.is_file() {
                return Some(path);
            }
            return EXTENSIONS
                .iter()
                .map(|ext| PathBuf::from(format!("{name}.{ext}")))
                .find(|p| p.is_file());
        }

        let base = EXTENSIONS
            .iter()
            .find_map(|ext| name.strip_suffix(&format!(".{ext}")))
            .unwrap_or(name);

        if let Some(path) = self
            .find_in_chain(base, size)
            .or_else(|| self.find_in_pixmaps(base))
        {
            return Some(path);
        }
        if let Some(path) = self.find_in_other_themes(base, size) {
            return Some(path);
        }
        let lower = base.to_ascii_lowercase();
        if lower != base {
            return self.locate(&lower, size);
        }
        STAND_INS
            .iter()
            .find(|(missing, _)| *missing == base)
            .and_then(|(_, stand_in)| self.locate(stand_in, size))
    }

    /// Walks the theme chain for the best sized file: a directory holding the size itself (or a
    /// scalable one covering it) in any theme of the chain wins over a nearer theme's other
    /// sizes, as the icon theme specification looks icons up, and where no theme holds the size
    /// the closest larger one across the chain is taken, a smaller one only where none is
    /// larger, since shrinking keeps an icon's edges and enlarging smears them: a 52 px slot
    /// takes a 64 px file over a 48 px one.
    fn find_in_chain(&self, name: &str, size: u32) -> Option<PathBuf> {
        /// Added to a smaller file's distance, so every larger file comes first.
        const UNDERSIZED: u32 = 1 << 20;
        let mut closest: Option<(u32, u32, u32, PathBuf)> = None;
        for (depth, theme) in self.chain.iter().enumerate() {
            let depth = depth as u32;
            let mut exact: Option<(u32, PathBuf)> = None;
            for dir in &theme.dirs {
                let matches = dir.matches(size);
                let distance = if matches {
                    0
                } else {
                    let physical = dir.size * dir.scale;
                    let distance = dir.distance(size);
                    if physical < size {
                        distance + UNDERSIZED
                    } else {
                        distance
                    }
                };
                for root in &theme.roots {
                    let folder = root.join(&dir.path);
                    for (rank, ext) in EXTENSIONS.iter().enumerate() {
                        let candidate = folder.join(format!("{name}.{ext}"));
                        if !candidate.is_file() {
                            continue;
                        }
                        let rank = rank as u32;
                        if matches && exact.as_ref().is_none_or(|(r, _)| rank < *r) {
                            exact = Some((rank, candidate.clone()));
                        }
                        let better = match &closest {
                            None => true,
                            Some((d, dp, r, _)) => {
                                distance < *d
                                    || (distance == *d && (depth < *dp || (depth == *dp && rank < *r)))
                            }
                        };
                        if better {
                            closest = Some((distance, depth, rank, candidate));
                        }
                    }
                }
            }
            if let Some((_, path)) = exact {
                return Some(path);
            }
        }
        closest.map(|(_, _, _, path)| path)
    }

    /// Looks in the pixmaps directories, which carry no size information
    /// beyond the `_WxH` suffix some packages append; the largest such file wins.
    fn find_in_pixmaps(&self, name: &str) -> Option<PathBuf> {
        for dir in &self.pixmap_dirs {
            for ext in EXTENSIONS {
                let candidate = dir.join(format!("{name}.{ext}"));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            let prefix = format!("{name}_");
            let mut best: Option<(u32, PathBuf)> = None;
            for entry in entries.flatten() {
                let file = entry.file_name();
                let Some(file) = file.to_str() else {
                    continue;
                };
                let Some(rest) = file.strip_prefix(&prefix) else {
                    continue;
                };
                let Some((dims, ext)) = rest.rsplit_once('.') else {
                    continue;
                };
                if !EXTENSIONS.contains(&ext) {
                    continue;
                }
                let Some(side) = dims.split_once('x').and_then(|(w, _)| w.parse::<u32>().ok()) else {
                    continue;
                };
                if best.as_ref().is_none_or(|(b, _)| side > *b) {
                    best = Some((side, entry.path()));
                }
            }
            if let Some((_, path)) = best {
                return Some(path);
            }
        }
        None
    }

    /// Searches every installed theme outside the chain, reading the size from
    /// the directory names rather than each theme's index.
    fn find_in_other_themes(&mut self, name: &str, size: u32) -> Option<PathBuf> {
        if self.other_themes.is_none() {
            let mut themes = Vec::new();
            for root in &self.search_roots {
                let Ok(entries) = std::fs::read_dir(root) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() && !self.chain.iter().any(|t| t.roots.contains(&path)) {
                        themes.push(path);
                    }
                }
            }
            self.other_themes = Some(themes);
        }
        let mut best: Option<(u32, PathBuf)> = None;
        for theme in self.other_themes.as_ref()? {
            let Ok(size_dirs) = std::fs::read_dir(theme) else {
                continue;
            };
            for size_dir in size_dirs.flatten() {
                let size_name = size_dir.file_name();
                let Some(size_name) = size_name.to_str() else {
                    continue;
                };
                let distance = match size_name {
                    "scalable" => 0,
                    _ => match size_name.split_once('x').and_then(|(w, _)| w.parse::<u32>().ok()) {
                        Some(w) => w.abs_diff(size),
                        None => continue,
                    },
                };
                if let Some((d, _)) = &best
                    && distance >= *d
                {
                    continue;
                }
                let Ok(contexts) = std::fs::read_dir(size_dir.path()) else {
                    continue;
                };
                for context in contexts.flatten() {
                    for ext in EXTENSIONS {
                        let candidate = context.path().join(format!("{name}.{ext}"));
                        if candidate.is_file() {
                            best = Some((distance, candidate));
                            break;
                        }
                    }
                }
            }
        }
        best.map(|(_, p)| p)
    }
}

/// Reads the GTK icon theme setting the user or the image may have left.
fn gtk_icon_theme() -> Option<String> {
    let candidates = [
        dirs::home().join(".config/gtk-3.0/settings.ini"),
        dirs::home().join(".config/gtk-4.0/settings.ini"),
        PathBuf::from("/etc/gtk-3.0/settings.ini"),
    ];
    for path in candidates {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in text.lines() {
            if let Some((key, value)) = line.split_once('=')
                && key.trim() == "gtk-icon-theme-name"
            {
                let value = value.trim().trim_matches('"');
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }
    None
}

/// Loads an icon file with the decoder its extension calls for and scales it
/// to the requested size.
fn load_file(path: &Path, size: u32) -> Option<Pixmap> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("svg") | Some("svgz") => render::load_svg(path, size),
        Some("xpm") => {
            let text = std::fs::read_to_string(path).ok()?;
            let pix = xpm::decode(&text)?;
            render::resize_pixmap(&pix, size)
        }
        _ => render::load_raster(path, size),
    }
}

/// Parses an `index.theme`: the subdirectories with their size rules, and the
/// themes it inherits from.
fn parse_index(text: &str) -> (Vec<SubDir>, Vec<String>) {
    let mut group = String::new();
    let mut listed: Vec<String> = Vec::new();
    let mut inherits: Vec<String> = Vec::new();
    let mut by_group: HashMap<String, HashMap<String, String>> = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            group = line[1..line.len() - 1].to_string();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if group == "Icon Theme" {
            match key {
                "Directories" | "ScaledDirectories" => {
                    listed.extend(
                        value
                            .split(',')
                            .map(|d| d.trim().to_string())
                            .filter(|d| !d.is_empty()),
                    );
                }
                "Inherits" => {
                    inherits.extend(
                        value
                            .split(',')
                            .map(|d| d.trim().to_string())
                            .filter(|d| !d.is_empty()),
                    );
                }
                _ => {}
            }
        } else {
            by_group
                .entry(group.clone())
                .or_default()
                .insert(key.to_string(), value.to_string());
        }
    }
    let mut dirs = Vec::new();
    for path in listed {
        let Some(keys) = by_group.get(&path) else {
            continue;
        };
        let Some(size) = keys.get("Size").and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let scale = keys
            .get("Scale")
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(1)
            .max(1);
        let kind = match keys.get("Type").map(|s| s.as_str()).unwrap_or("Threshold") {
            "Fixed" => DirType::Fixed,
            "Scalable" => DirType::Scalable {
                min: keys.get("MinSize").and_then(|s| s.parse().ok()).unwrap_or(size),
                max: keys.get("MaxSize").and_then(|s| s.parse().ok()).unwrap_or(size),
            },
            _ => DirType::Threshold(keys.get("Threshold").and_then(|s| s.parse().ok()).unwrap_or(2)),
        };
        dirs.push(SubDir {
            path,
            size,
            scale,
            kind,
        });
    }
    (dirs, inherits)
}

#[cfg(test)]
mod tests {
    use super::*;

    const INDEX: &str = "[Icon Theme]\nName=Test\nInherits=hicolor,Adwaita\nDirectories=16x16/apps,scalable/apps,48x48@2/apps\n\n[16x16/apps]\nSize=16\nType=Fixed\n\n[scalable/apps]\nSize=128\nType=Scalable\nMinSize=8\nMaxSize=512\n\n[48x48@2/apps]\nSize=48\nScale=2\nType=Threshold\n";

    /// A theme later in the chain that holds the size asked for wins over an earlier theme's
    /// other sizes, and where no theme holds it a larger file is taken over a smaller one, even
    /// a nearer smaller one.
    #[test]
    fn the_chain_prefers_an_exact_size_anywhere_and_then_the_larger_file() {
        let base = std::env::temp_dir().join(format!("selkies-icons-{}", std::process::id()));
        let first = base.join("first/16x16/apps");
        let second = base.join("second/32x32/apps");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(first.join("browser.png"), b"png").unwrap();
        std::fs::write(second.join("browser.png"), b"png").unwrap();
        let fixed = |size: u32, path: &str| SubDir {
            path: path.to_string(),
            size,
            scale: 1,
            kind: DirType::Fixed,
        };
        let icons = Icons {
            search_roots: Vec::new(),
            chain: vec![
                Theme {
                    roots: vec![base.join("first")],
                    dirs: vec![fixed(16, "16x16/apps")],
                },
                Theme {
                    roots: vec![base.join("second")],
                    dirs: vec![fixed(32, "32x32/apps")],
                },
            ],
            pixmap_dirs: Vec::new(),
            cache: HashMap::new(),
            other_themes: Some(Vec::new()),
        };
        assert_eq!(
            icons.find_in_chain("browser", 32),
            Some(second.join("browser.png"))
        );
        assert_eq!(
            icons.find_in_chain("browser", 16),
            Some(first.join("browser.png"))
        );
        assert_eq!(
            icons.find_in_chain("browser", 24),
            Some(second.join("browser.png"))
        );
        assert_eq!(
            icons.find_in_chain("browser", 20),
            Some(second.join("browser.png"))
        );
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn parses_index_theme() {
        let (dirs, inherits) = parse_index(INDEX);
        assert_eq!(inherits, vec!["hicolor".to_string(), "Adwaita".to_string()]);
        assert_eq!(dirs.len(), 3);
        assert!(dirs[0].matches(16));
        assert!(!dirs[0].matches(32));
        assert_eq!(dirs[0].distance(32), 16);
        assert!(dirs[1].matches(16));
        assert!(dirs[1].matches(500));
        assert!(dirs[2].matches(96));
        assert_eq!(dirs[2].distance(200), 200 - 100);
    }
}
