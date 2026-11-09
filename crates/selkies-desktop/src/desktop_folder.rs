/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The Desktop folder the wallpaper shows: its launchers, folders, and files, and where each
//! stands on the icon grid.
//!
//! Every visible entry of the user's Desktop directory is an item, the way the file manager
//! lists the folder: a `.desktop` launcher shows its application's name and icon and starts
//! it, a folder opens in the file manager, and any other file opens in the application its
//! type names. Folders come first, then everything by name, which is the order new items fill
//! the grid in.
//!
//! The grid is columns of cells top to bottom; where an item stands is a cell, kept by file
//! name in `desktop-layout` beside the shell's settings file, one `column`, `row`, and name
//! per line, tab-separated. An item the file places on a free cell inside the grid keeps it;
//! the rest take the first free cells in column order, so a screen that shrinks folds the
//! items it can no longer hold back onto it and one that grows does not move them. Dragging
//! an icon writes every item's cell, so the layout the user sees is the one restored.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use selkies_draw::apps::Entry;

use crate::files::{self, Item, MimeDb};

/// One item of the Desktop folder.
#[derive(Clone, Debug)]
pub struct DesktopItem {
    /// The file name in the Desktop directory, what the layout keeps a cell by.
    pub file: String,
    /// The name shown under the icon.
    pub name: String,
    /// The launcher a `.desktop` file describes, if it is one.
    pub launcher: Option<Entry>,
    /// The file as the file manager lists it.
    pub item: Item,
}

/// A cell of the icon grid: a column from the left and a row from the top.
pub type Cell = (i32, i32);

/// The items of a Desktop directory: hidden entries left out, a launcher wherever the shell's
/// application list parsed one, folders first and then by name, case aside.
///
/// # Arguments
/// * `dir` - The Desktop directory.
/// * `launchers` - The launchers the application list read from it.
/// * `mime` - The type database the file manager names types with.
pub fn load(dir: &Path, launchers: &[Entry], mime: &MimeDb) -> Vec<DesktopItem> {
    let Ok(listed) = files::read_dir(dir, mime) else {
        return Vec::new();
    };
    let mut items: Vec<DesktopItem> = listed
        .into_iter()
        .filter(|item| !item.hidden)
        .map(|item| {
            let launcher = launchers.iter().find(|e| e.path == item.path).cloned();
            DesktopItem {
                file: item.name.clone(),
                name: launcher
                    .as_ref()
                    .map_or_else(|| item.name.clone(), |e| e.name.clone()),
                launcher,
                item,
            }
        })
        .collect();
    items.sort_by(|a, b| {
        b.item
            .is_dir
            .cmp(&a.item.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    items
}

/// The file the grid layout is kept in, beside the shell's settings file.
pub fn layout_path() -> PathBuf {
    selkies_draw::style::desktop_config_dir().join("desktop-layout")
}

/// The cells a layout text keeps, by file name; a line that does not read is skipped.
///
/// # Arguments
/// * `text` - The layout file's contents.
pub fn parse_layout(text: &str) -> HashMap<String, Cell> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, '\t');
            let column = fields.next()?.trim().parse().ok()?;
            let row = fields.next()?.trim().parse().ok()?;
            let name = fields.next()?;
            (!name.is_empty()).then(|| (name.to_string(), (column, row)))
        })
        .collect()
}

/// The layout text for the cells of a list of items.
pub fn format_layout(items: &[DesktopItem], cells: &[Cell]) -> String {
    items
        .iter()
        .zip(cells)
        .map(|(item, (column, row))| format!("{column}\t{row}\t{}\n", item.file))
        .collect()
}

/// Reads the saved layout, empty where there is none.
pub fn read_layout() -> HashMap<String, Cell> {
    std::fs::read_to_string(layout_path())
        .map(|text| parse_layout(&text))
        .unwrap_or_default()
}

/// Writes the layout of a list of items.
pub fn write_layout(items: &[DesktopItem], cells: &[Cell]) -> std::io::Result<()> {
    let path = layout_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format_layout(items, cells))
}

/// Where each item stands on a grid of `rows` cells a column: its saved cell where that is
/// inside the grid's `columns` and not taken by an item placed before it, else the first free
/// cell in column order.
///
/// # Arguments
/// * `items` - The items, in their listing order.
/// * `saved` - The saved cells by file name.
/// * `columns` - How many columns fit on the screen.
/// * `rows` - How many cells a column holds.
pub fn place(items: &[DesktopItem], saved: &HashMap<String, Cell>, columns: i32, rows: i32) -> Vec<Cell> {
    let rows = rows.max(1);
    let columns = columns.max(1);
    let mut taken: HashSet<Cell> = HashSet::new();
    let mut cells: Vec<Option<Cell>> = vec![None; items.len()];
    for (index, item) in items.iter().enumerate() {
        if let Some(&(column, row)) = saved.get(&item.file)
            && (0..columns).contains(&column)
            && (0..rows).contains(&row)
            && taken.insert((column, row))
        {
            cells[index] = Some((column, row));
        }
    }
    let mut next = 0;
    cells
        .into_iter()
        .map(|cell| {
            cell.unwrap_or_else(|| {
                loop {
                    let candidate = (next / rows, next % rows);
                    next += 1;
                    if taken.insert(candidate) {
                        break candidate;
                    }
                }
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, is_dir: bool) -> DesktopItem {
        DesktopItem {
            file: name.to_string(),
            name: name.to_string(),
            launcher: None,
            item: Item {
                name: name.to_string(),
                path: PathBuf::from("/tmp").join(name),
                is_dir,
                is_symlink: false,
                size: 0,
                modified: 0,
                mime: String::new(),
                hidden: false,
                executable: false,
            },
        }
    }

    /// Saved cells inside the grid are kept, a clash and a cell off the grid fall back to the
    /// first free cells in column order, and the text round-trips.
    #[test]
    fn items_keep_their_saved_cells_and_the_rest_fill_free_ones() {
        let items = vec![
            item("a", true),
            item("b", false),
            item("c", false),
            item("d", false),
        ];
        let saved = parse_layout("2\t1\tb\n2\t1\tc\n9\t0\td\nbad line\n");
        let cells = place(&items, &saved, 4, 3);
        assert_eq!(cells, vec![(0, 0), (2, 1), (0, 1), (0, 2)]);
        assert_eq!(
            parse_layout(&format_layout(&items, &cells)).get("b"),
            Some(&(2, 1))
        );
        assert_eq!(
            parse_layout("1\t0\tname\twith tab").get("name\twith tab"),
            Some(&(1, 0))
        );
    }

    /// A Desktop directory lists folders first, then files by name, with dotfiles left out.
    #[test]
    fn a_desktop_directory_lists_folders_first_and_hides_dotfiles() {
        let dir = std::env::temp_dir().join(format!("selkies-desktop-folder-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("zeta")).unwrap();
        std::fs::write(dir.join("alpha.txt"), b"a").unwrap();
        std::fs::write(dir.join(".hidden"), b"h").unwrap();
        let items = load(&dir, &[], &MimeDb::default());
        let names: Vec<&str> = items.iter().map(|i| i.file.as_str()).collect();
        assert_eq!(names, vec!["zeta", "alpha.txt"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
