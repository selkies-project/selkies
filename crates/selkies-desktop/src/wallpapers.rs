/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The wallpapers installed on the system and for the user, and the image of one a monitor
//! shows.
//!
//! A wallpaper is an image file or a wallpaper package: a folder holding `contents/images/`
//! with the light pictures and, where it has them, `contents/images_dark/` with the dark ones,
//! each picture named for its size (`5120x2880.png`), and a `metadata.json` (or an older
//! `metadata.desktop`) naming it and its author. The list reads the `wallpapers` folder of
//! every data directory, the user's own (`~/.local/share`) first and then the system's
//! (`$XDG_DATA_DIRS`, `/usr/local/share` and `/usr/share` by default): every package there and
//! every loose image, and the loose images of the `backgrounds` folders and their subfolders.
//!
//! A monitor shows a package's light or dark pictures as the theme or the user's choice says,
//! and of those the one whose shape is closest to the monitor's, the smallest of them that
//! covers it, since a package ships pictures for screens up to several times a desktop's size
//! and the shell decodes the one it shows whole. A package without the variant asked for
//! shows the other.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// The image files a wallpaper can be, by extension.
const IMAGE_EXTENSIONS: [&str; 3] = ["png", "jpg", "jpeg"];

/// How far a picture's aspect ratio may stray from the best one and still count as that
/// shape.
const ASPECT_SLACK: f64 = 0.05;

/// The size a thumbnail's pictures are chosen for.
const THUMBNAIL_TARGET: (u32, u32) = (480, 270);

/// One wallpaper to choose from.
#[derive(Clone, Debug, PartialEq)]
pub struct WallpaperEntry {
    /// What the setting names: the image file, or the package's folder.
    pub path: PathBuf,
    /// The name shown under its thumbnail.
    pub name: String,
    /// Who made it, where its metadata says.
    pub author: Option<String>,
    /// Whether it has dark pictures of its own.
    pub has_dark: bool,
    /// The path with its links resolved, which a setting naming it through a link matches.
    pub real: PathBuf,
    /// The light picture its thumbnail shows.
    pub light: Option<PathBuf>,
    /// The dark picture its thumbnail shows, where it has dark ones.
    pub dark: Option<PathBuf>,
}

/// The data directories wallpapers are read from, the user's first.
pub fn data_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let user = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join(".local/share"));
    let system = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());
    let mut dirs = vec![user];
    dirs.extend(system.split(':').filter(|p| !p.is_empty()).map(PathBuf::from));
    let mut seen = HashSet::new();
    dirs.retain(|dir| seen.insert(dir.clone()));
    dirs
}

/// Whether a path is a wallpaper package.
pub fn is_package(path: &Path) -> bool {
    path.join("contents/images").is_dir() || path.join("contents/images_dark").is_dir()
}

/// Whether a path is an image file a wallpaper can be.
fn is_image(path: &Path) -> bool {
    path.is_file()
        && path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| IMAGE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// Every wallpaper installed, the user's first, then by name.
pub fn list() -> Vec<WallpaperEntry> {
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    for dir in data_dirs() {
        let mut found = Vec::new();
        for child in read_sorted(&dir.join("wallpapers")) {
            if is_package(&child) {
                found.push(package_entry(&child));
            } else if is_image(&child) {
                found.push(image_entry(&child));
            }
        }
        for child in read_sorted(&dir.join("backgrounds")) {
            if is_image(&child) {
                found.push(image_entry(&child));
            } else if child.is_dir() {
                found.extend(
                    read_sorted(&child)
                        .into_iter()
                        .filter(|p| is_image(p))
                        .map(|p| image_entry(&p)),
                );
            }
        }
        found.sort_by_key(|entry| entry.name.to_lowercase());
        for entry in found {
            if seen.insert(entry.real.clone()) {
                entries.push(entry);
            }
        }
    }
    entries
}

/// A folder's children by name, none where it cannot be read.
fn read_sorted(dir: &Path) -> Vec<PathBuf> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = read
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            !p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with('.'))
        })
        .collect();
    paths.sort();
    paths
}

/// The entry of a loose image, named for its file.
fn image_entry(path: &Path) -> WallpaperEntry {
    WallpaperEntry {
        path: path.to_path_buf(),
        name: path
            .file_stem()
            .map(|s| s.to_string_lossy().replace(['_', '-'], " "))
            .unwrap_or_default(),
        author: None,
        has_dark: false,
        real: real_path(path),
        light: Some(path.to_path_buf()),
        dark: None,
    }
}

/// A path with its links resolved, the path itself where that fails.
pub fn real_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The entry of a package, named as its metadata says, else for its folder.
fn package_entry(dir: &Path) -> WallpaperEntry {
    let (name, author) = read_metadata(dir);
    let has_dark = !pictures(dir, true).is_empty();
    WallpaperEntry {
        path: dir.to_path_buf(),
        name: name.unwrap_or_else(|| {
            dir.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        }),
        author,
        has_dark,
        real: real_path(dir),
        light: resolve(dir, false, THUMBNAIL_TARGET),
        dark: has_dark
            .then(|| pick(pictures(dir, true), THUMBNAIL_TARGET))
            .flatten(),
    }
}

/// A package's name and author from its `metadata.json`, else its `metadata.desktop`.
fn read_metadata(dir: &Path) -> (Option<String>, Option<String>) {
    if let Ok(text) = std::fs::read_to_string(dir.join("metadata.json"))
        && let Ok(json) = serde_json::from_str::<serde_json::Value>(&text)
    {
        let plugin = &json["KPlugin"];
        let name = plugin["Name"].as_str().map(str::to_string);
        let author = plugin["Authors"]
            .get(0)
            .and_then(|a| a["Name"].as_str())
            .map(str::to_string);
        return (name, author);
    }
    let Ok(text) = std::fs::read_to_string(dir.join("metadata.desktop")) else {
        return (None, None);
    };
    let field = |key: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    (field("Name"), field("X-KDE-PluginInfo-Author"))
}

/// A package's pictures of one variant, each with the size its file name gives, `(0, 0)`
/// where the name gives none.
fn pictures(dir: &Path, dark: bool) -> Vec<(PathBuf, (u32, u32))> {
    let folder = dir.join(if dark {
        "contents/images_dark"
    } else {
        "contents/images"
    });
    read_sorted(&folder)
        .into_iter()
        .filter(|p| is_image(p))
        .map(|p| {
            let size = p
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.split_once('x'))
                .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
                .unwrap_or((0, 0));
            (p, size)
        })
        .collect()
}

/// The picture for a target size: of those whose shape is closest to the target's, the
/// smallest that covers it, else the largest; an unsized picture only when no other is there.
fn pick(pictures: Vec<(PathBuf, (u32, u32))>, target: (u32, u32)) -> Option<PathBuf> {
    let aspect = |(w, h): (u32, u32)| w as f64 / h.max(1) as f64;
    let wanted = aspect((target.0.max(1), target.1.max(1)));
    let sized: Vec<_> = pictures.iter().filter(|(_, s)| s.0 > 0 && s.1 > 0).collect();
    if sized.is_empty() {
        return pictures.into_iter().next().map(|(p, _)| p);
    }
    let distance = |size: (u32, u32)| (aspect(size) / wanted).ln().abs();
    let best = sized
        .iter()
        .map(|(_, s)| distance(*s))
        .fold(f64::INFINITY, f64::min);
    let shaped: Vec<_> = sized
        .into_iter()
        .filter(|(_, s)| distance(*s) <= best + ASPECT_SLACK)
        .collect();
    let covering = shaped
        .iter()
        .filter(|(_, s)| s.0 >= target.0 && s.1 >= target.1)
        .min_by_key(|(_, s)| s.0 as u64 * s.1 as u64);
    covering
        .or_else(|| shaped.iter().max_by_key(|(_, s)| s.0 as u64 * s.1 as u64))
        .map(|(p, _)| p.clone())
}

/// The image file a wallpaper shows: an image file as it is, a package's picture of the
/// variant asked for, or of the other where it has none.
///
/// # Arguments
/// * `path` - The image file or the package's folder.
/// * `dark` - Whether the dark variant is wanted.
/// * `target` - The size in physical pixels the picture fills.
pub fn resolve(path: &Path, dark: bool, target: (u32, u32)) -> Option<PathBuf> {
    if path.is_file() {
        return Some(path.to_path_buf());
    }
    if !is_package(path) {
        return None;
    }
    pick(pictures(path, dark), target).or_else(|| pick(pictures(path, !dark), target))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temporary folder for a test, removed and made anew.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("selkies-wallpapers-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A package's picture follows the shape of the screen and the smallest that covers it,
    /// the dark ones for the dark variant and the light ones where it has no dark.
    #[test]
    fn a_package_shows_the_picture_that_fits_the_variant_and_the_screen() {
        let dir = scratch("pick");
        let package = dir.join("Next");
        for folder in ["contents/images", "contents/images_dark"] {
            std::fs::create_dir_all(package.join(folder)).unwrap();
            for name in ["1440x2960.png", "5120x2880.png", "7680x2160.png", "2560x1440.png"] {
                std::fs::write(package.join(folder).join(name), b"").unwrap();
            }
        }
        std::fs::write(
            package.join("metadata.json"),
            r#"{"KPlugin": {"Name": "Next", "Authors": [{"Name": "Someone"}]}}"#,
        )
        .unwrap();
        let file = |p: Option<PathBuf>| p.and_then(|p| Some(p.strip_prefix(&package).ok()?.to_path_buf()));
        assert_eq!(
            file(resolve(&package, true, (1920, 1080))),
            Some(PathBuf::from("contents/images_dark/2560x1440.png"))
        );
        assert_eq!(
            file(resolve(&package, false, (3840, 2160))),
            Some(PathBuf::from("contents/images/5120x2880.png"))
        );
        assert_eq!(
            file(resolve(&package, false, (3440, 960))),
            Some(PathBuf::from("contents/images/7680x2160.png"))
        );
        std::fs::remove_dir_all(package.join("contents/images_dark")).unwrap();
        assert_eq!(
            file(resolve(&package, true, (1080, 2220))),
            Some(PathBuf::from("contents/images/1440x2960.png"))
        );
        let entry = package_entry(&package);
        assert_eq!(
            (entry.name.as_str(), entry.author.as_deref()),
            ("Next", Some("Someone"))
        );
        assert!(!entry.has_dark);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
