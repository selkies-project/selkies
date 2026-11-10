//! The start menu: a search box, the categories with favorites and recent
//! launches, and the applications of the chosen category, in one of three
//! layouts.  Columns lists the applications beside the categories, each a
//! row with a 32 px icon over its name and description, and the favorites as
//! a grid of large icons; Grid shows every list as a grid of icons; and Tiles
//! drops the categories for a list of every application with the favorites
//! as tiles beside it.  Each category row carries its theme icon.  The
//! shell's own settings open from a button beside the search box rather
//! than from a category, so the categories list the applications alone.
//! An application's right-click menu pins it to the favorites or the quick
//! launch and puts its launcher on the desktop.  A category opens on a
//! click, so the pointer can cross the column to the applications without
//! changing the list, and a column whose content overflows shows a thin
//! scrollbar whose thumb drags and whose track jumps there, since a pointer
//! may have no wheel.  A favorite launches on its release, so one held and
//! dragged moves to the place it is dropped on instead, in the favorites'
//! grid and among the tiles alike.

use crate::config::MenuLayout;
use crate::desktop::{Desktop, Hover, Popup};
use crate::entries;
use crate::event::Button;
use crate::geom::Rect;
use crate::i18n::{tr, trf};
use crate::render::{Align, Canvas};
use crate::ui;
use crate::xkb::Key;

/// Menu width.
const WIDTH: i32 = 640;
/// Height of the menu.
const HEIGHT: i32 = 500;
/// Shortest the menu shrinks on a small screen.
const MIN_HEIGHT: i32 = 200;
/// Width of the category column.
const CATEGORY_WIDTH: i32 = 210;
/// Height of a category row.
const CATEGORY_ROW: i32 = 34;
/// Icon size on a category row.
const CATEGORY_ICON: i32 = 20;
/// Width of the application list in the tiles layout.
const LIST_WIDTH: i32 = 210;
/// Height of an application row: the icon beside the name over the description.
const ROW: i32 = 44;
/// Icon size on an application row.
const ROW_ICON: i32 = 32;
/// Width of a cell of the application grid.
const CELL_W: i32 = 72;
/// Height of a cell of the application grid.
const CELL_H: i32 = 70;
/// Icon size in the application grid.
const CELL_ICON: i32 = 32;
/// Narrowest a cell of the favorites' grid gets, four of them fitting the application
/// column beside its scrollbar's gutter.
const FAVORITE_W: i32 = 96;
/// Height of a cell of the favorites' grid, room for a two-line name.
const FAVORITE_H: i32 = 108;
/// Icon size in the favorites' grid.
const FAVORITE_ICON: i32 = 52;
/// Side of a favorite's tile.
const TILE: i32 = 80;
/// Space between tiles.
const TILE_GAP: i32 = 6;
/// Icon size on a tile.
const TILE_ICON: i32 = 34;
/// Inner padding.
const PAD: i32 = 10;
/// Height of the search box.
const SEARCH_HEIGHT: i32 = 34;
/// Width of the Settings button, room for its icon and a translated label.
const SETTINGS_WIDTH: i32 = 140;
/// Height of the footer.
const FOOTER_HEIGHT: i32 = 30;
/// Width of an application's right-click menu.
const CONTEXT_WIDTH: i32 = 230;
/// Row height of an application's right-click menu.
const CONTEXT_ROW: i32 = 30;
/// Width of a scrollbar's thumb, wide enough to survive the stream's chroma subsampling.
const SCROLLBAR_W: i32 = 6;
/// Width of the strip along a column's right edge that takes the scrollbar's clicks.
const SCROLLBAR_HIT: i32 = 14;
/// Shortest a scrollbar's thumb gets.
const SCROLLBAR_MIN_THUMB: i32 = 28;
/// Width kept free along a scrolling column's right edge for its scrollbar, shown or not, so
/// the bar never covers a row and the content does not reflow when it appears.
const SCROLL_GUTTER: i32 = SCROLLBAR_W + 4;

/// How far a held favorite moves before it drags rather than launches.
const FAVORITE_DRAG_THRESHOLD: f64 = 6.0;

/// A favorite held by the pointer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FavoriteDrag {
    /// Its place among the favorites shown.
    pub from: usize,
    /// Where the press was.
    pub start: (f64, f64),
    /// Where the pointer is.
    pub at: (f64, f64),
    /// Whether it moved far enough to drag.
    pub dragging: bool,
}

/// A column of the menu that scrolls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollColumn {
    Categories,
    Apps,
}

/// Which list the application column shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    /// The configured favorites.
    Favorites,
    /// Recently launched applications.
    Recent,
    /// Every application.
    All,
    /// One category, by index.
    Category(usize),
}

/// A row of the category column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CategoryRow {
    Section(Section),
    Separator,
}

/// The menu's state between events.
pub struct MenuState {
    /// The search text.
    pub query: String,
    /// The section whose applications are listed.
    pub section: Section,
    /// Row of the application list selected with the keyboard.
    pub selected: Option<usize>,
    /// Scroll offset of the category column.
    pub category_scroll: f64,
    /// Scroll offset of the application column.
    pub app_scroll: f64,
    /// The right-click menu open on an application, with its entry index
    /// and where it opened.
    pub context: Option<(usize, i32, i32)>,
    /// The scrollbar whose thumb is being dragged, with where in the thumb it was taken.
    pub scroll_drag: Option<(ScrollColumn, f64)>,
    /// The favorite held since its press, until the release launches or moves it.
    pub favorite_drag: Option<FavoriteDrag>,
    items: Vec<usize>,
    items_valid: bool,
    recent_ids: Vec<String>,
    identity: String,
}

impl Default for MenuState {
    fn default() -> MenuState {
        MenuState {
            query: String::new(),
            section: Section::All,
            selected: None,
            category_scroll: 0.0,
            app_scroll: 0.0,
            context: None,
            scroll_drag: None,
            favorite_drag: None,
            items: Vec::new(),
            items_valid: false,
            recent_ids: Vec::new(),
            identity: identity(),
        }
    }
}

impl MenuState {
    /// Forgets the computed application list.
    pub fn invalidate(&mut self) {
        self.items_valid = false;
    }

    /// Resets the menu for opening.
    ///
    /// # Arguments
    /// * `d` - The desktop, whose applications fill the lists.
    pub fn open(&mut self, d: &Desktop) {
        self.query.clear();
        self.selected = None;
        self.category_scroll = 0.0;
        self.app_scroll = 0.0;
        self.recent_ids = if d.config.track_recent {
            entries::recent()
        } else {
            Vec::new()
        };
        let favorites = favorite_entries(d);
        let recent: Vec<usize> = self.recent_ids.iter().filter_map(|id| d.apps.find(id)).collect();
        self.section = if d.config.menu_layout == MenuLayout::Tiles {
            Section::All
        } else if !favorites.is_empty() {
            Section::Favorites
        } else if !recent.is_empty() {
            Section::Recent
        } else {
            Section::All
        };
        self.items_valid = false;
    }

    /// Clears transient state when the menu closes.
    pub fn close(&mut self) {
        self.query.clear();
        self.selected = None;
        self.context = None;
        self.scroll_drag = None;
        self.favorite_drag = None;
    }

    /// The application index shown at a row, when the list is current.
    ///
    /// # Arguments
    /// * `_d` - The desktop.
    /// * `row` - Row of the application list.
    pub fn visible_entry(&self, _d: &Desktop, row: usize) -> Option<usize> {
        if self.items_valid {
            self.items.get(row).copied()
        } else {
            None
        }
    }
}

/// `user@host` for the footer.
fn identity() -> String {
    let user = unsafe {
        let entry = libc::getpwuid(libc::getuid());
        if entry.is_null() || (*entry).pw_name.is_null() {
            None
        } else {
            Some(
                std::ffi::CStr::from_ptr((*entry).pw_name)
                    .to_string_lossy()
                    .to_string(),
            )
        }
    };
    let user = user
        .filter(|u| !u.is_empty())
        .or_else(|| std::env::var("USER").ok().filter(|u| !u.is_empty()))
        .unwrap_or_else(|| "user".to_string());
    let mut buf = [0u8; 256];
    let host = unsafe {
        if libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) == 0 {
            std::ffi::CStr::from_ptr(buf.as_ptr() as *const libc::c_char)
                .to_string_lossy()
                .to_string()
        } else {
            String::new()
        }
    };
    if host.is_empty() {
        user
    } else {
        format!("{user}@{host}")
    }
}

/// The rectangles of the menu.
struct Layout {
    frame: Rect,
    search: Rect,
    /// The Settings button at the right end of the search row.
    settings: Rect,
    /// The category column, zero wide while searching and in the tiles
    /// layout.
    categories: Rect,
    /// The application rows or grid.
    apps: Rect,
    /// The favorites' tiles, zero wide outside the tiles layout and while
    /// searching.
    tiles: Rect,
    footer: Rect,
}

/// How the application area lays its items out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Arrangement {
    /// One row an application.
    Rows,
    /// A grid of icons with their names beneath, cells of a size.
    Grid { w: i32, h: i32, icon: i32 },
}

/// The arrangement of the application area: a grid in the grid layout, the
/// favorites as a grid of large icons in the columns layout while nothing is
/// searched, rows otherwise.
fn arrangement(d: &Desktop) -> Arrangement {
    let searching = !d.menu.query.trim().is_empty();
    match d.config.menu_layout {
        MenuLayout::Grid => Arrangement::Grid {
            w: CELL_W,
            h: CELL_H,
            icon: CELL_ICON,
        },
        MenuLayout::Columns if d.menu.section == Section::Favorites && !searching => Arrangement::Grid {
            w: FAVORITE_W,
            h: FAVORITE_H,
            icon: FAVORITE_ICON,
        },
        _ => Arrangement::Rows,
    }
}

/// The theme icon a section's row carries.
fn section_icon(d: &Desktop, section: Section) -> &'static str {
    match section {
        Section::Favorites => "bookmarks",
        Section::Recent => "document-open-recent",
        Section::All => "applications-all",
        Section::Category(i) => match d.apps.categories.get(i).map(|c| c.name.as_str()) {
            Some("Multimedia") => "applications-multimedia",
            Some("Development") => "applications-development",
            Some("Education") => "applications-education",
            Some("Games") => "applications-games",
            Some("Graphics") => "applications-graphics",
            Some("Internet") => "applications-internet",
            Some("Office") => "applications-office",
            Some("Science") => "applications-science",
            Some("Settings") => "preferences-system",
            Some("System") => "applications-system",
            Some("Utilities") => "applications-utilities",
            _ => "applications-other",
        },
    }
}

/// Computes the menu rectangles for the current panel size.  The menu has a
/// fixed height so it stays put while the lists change; the panel surface
/// may be taller than the screen, so the screen height, which the background
/// surface knows, bounds it.
fn layout(d: &Desktop) -> Layout {
    let bar = d.bar_rect();
    let screen_height = d
        .primary_background()
        .filter(|b| b.geometry.configured)
        .map_or(d.panel.height, |b| b.icon_area().h.min(d.panel.height));
    let searching = !d.menu.query.trim().is_empty();
    let height = HEIGHT
        .min(screen_height - d.reserved_height() - ui::POPUP_MARGIN * 2)
        .max(MIN_HEIGHT);
    let y = if d.panel_top() {
        bar.bottom() + ui::POPUP_MARGIN
    } else {
        bar.y - ui::POPUP_MARGIN - height
    };
    let width = WIDTH.min(d.panel.width - 2 * ui::POPUP_MARGIN);
    let x = (bar.x + ui::POPUP_MARGIN).min(d.panel.width - ui::POPUP_MARGIN - width);
    let frame = Rect::new(x.max(ui::POPUP_MARGIN), y, width, height);
    let settings = Rect::new(
        frame.right() - PAD - SETTINGS_WIDTH,
        frame.y + PAD,
        SETTINGS_WIDTH,
        SEARCH_HEIGHT,
    );
    let search = Rect::new(
        frame.x + PAD,
        frame.y + PAD,
        settings.x - PAD / 2 - frame.x - PAD,
        SEARCH_HEIGHT,
    );
    let footer = Rect::new(frame.x, frame.bottom() - FOOTER_HEIGHT, frame.w, FOOTER_HEIGHT);
    let content_top = search.bottom() + PAD;
    let content_h = footer.y - content_top - 4;
    let tiles_layout = d.config.menu_layout == MenuLayout::Tiles;
    let categories = if searching || tiles_layout {
        Rect::new(frame.x + PAD, content_top, 0, content_h)
    } else {
        Rect::new(frame.x + PAD, content_top, CATEGORY_WIDTH, content_h)
    };
    let apps_x = if searching || tiles_layout {
        frame.x + PAD
    } else {
        categories.right() + PAD
    };
    let apps_w = if tiles_layout && !searching {
        LIST_WIDTH
    } else {
        frame.right() - PAD - apps_x
    };
    let apps = Rect::new(apps_x, content_top, apps_w, content_h);
    let tiles = if tiles_layout && !searching {
        let x = apps.right() + PAD;
        Rect::new(x, content_top, frame.right() - PAD - x, content_h)
    } else {
        Rect::new(apps.right(), content_top, 0, content_h)
    };
    Layout {
        frame,
        search,
        settings,
        categories,
        apps,
        tiles,
        footer,
    }
}

/// The rows of the category column for the loaded applications.
fn category_rows(d: &Desktop) -> Vec<CategoryRow> {
    let mut rows = Vec::new();
    if d.config.favorites.iter().any(|id| d.apps.find(id).is_some()) {
        rows.push(CategoryRow::Section(Section::Favorites));
    }
    if d.menu.recent_ids.iter().any(|id| d.apps.find(id).is_some()) {
        rows.push(CategoryRow::Section(Section::Recent));
    }
    rows.push(CategoryRow::Section(Section::All));
    if !d.apps.categories.is_empty() {
        rows.push(CategoryRow::Separator);
    }
    for i in 0..d.apps.categories.len() {
        if !category_entries(d, i).is_empty() {
            rows.push(CategoryRow::Section(Section::Category(i)));
        }
    }
    rows
}

/// The entries a category lists: its applications without the shell's own settings, which the
/// Settings button opens, so a category holding nothing else is not listed.
fn category_entries(d: &Desktop, index: usize) -> Vec<usize> {
    let Some(category) = d.apps.categories.get(index) else {
        return Vec::new();
    };
    category
        .entries
        .iter()
        .copied()
        .filter(|&entry| {
            d.apps
                .entries
                .get(entry)
                .is_some_and(|e| e.id != entries::SETTINGS_ID)
        })
        .collect()
}

/// The entry indexes of the favorites that exist, in their configured order.
///
/// # Arguments
/// * `d` - The desktop.
pub fn favorite_entries(d: &Desktop) -> Vec<usize> {
    d.config
        .favorites
        .iter()
        .filter_map(|id| d.apps.find(id))
        .collect()
}

/// Display name of a section.
fn section_name(d: &Desktop, section: Section) -> String {
    match section {
        Section::Favorites => tr("Favorites").to_string(),
        Section::Recent => tr("Recent").to_string(),
        Section::All => tr("All applications").to_string(),
        Section::Category(i) => d
            .apps
            .categories
            .get(i)
            .map(|c| entries::category_name(&c.name).to_string())
            .unwrap_or_default(),
    }
}

/// Recomputes the application list when the query, section or applications changed.
fn ensure_items(d: &mut Desktop) {
    if d.menu.items_valid {
        return;
    }
    let query = d.menu.query.trim().to_string();
    if d.config.menu_layout == MenuLayout::Tiles {
        d.menu.section = Section::All;
    }
    if matches!(d.menu.section, Section::Category(_))
        && !category_rows(d).contains(&CategoryRow::Section(d.menu.section))
    {
        d.menu.section = Section::All;
    }
    let items = if !query.is_empty() {
        d.apps.search(&query)
    } else {
        match d.menu.section {
            Section::Favorites => favorite_entries(d),
            Section::Recent => d
                .menu
                .recent_ids
                .iter()
                .filter_map(|id| d.apps.find(id))
                .collect(),
            Section::All => d.apps.all_sorted(),
            Section::Category(i) => category_entries(d, i),
        }
    };
    d.menu.items = items;
    d.menu.items_valid = true;
    if let Some(selected) = d.menu.selected
        && selected >= d.menu.items.len()
    {
        d.menu.selected = if d.menu.items.is_empty() {
            None
        } else {
            Some(d.menu.items.len() - 1)
        };
    }
}

/// Columns of a grid of cells this wide in an area.
fn grid_columns(apps: Rect, cell_w: i32) -> usize {
    (content_width(apps) / cell_w).max(1) as usize
}

/// The width of a column's content, its scrollbar's gutter left out.
fn content_width(column: Rect) -> i32 {
    (column.w - SCROLL_GUTTER).max(1)
}

/// The width each cell of a grid takes: the column's content shared evenly among as many cells
/// of at least `cell_w` as fit, so the grid spans the column rather than leaving the slack at
/// its right.
fn grid_cell_width(apps: Rect, cell_w: i32) -> i32 {
    content_width(apps) / grid_columns(apps, cell_w) as i32
}

/// The cell of an item in the application grid, scrolled.
///
/// # Arguments
/// * `d` - The desktop.
/// * `apps` - The application area.
/// * `row` - The item's position in the list.
/// * `w` - The cell width.
/// * `h` - The cell height.
fn grid_cell(d: &Desktop, apps: Rect, row: usize, w: i32, h: i32) -> Rect {
    let columns = grid_columns(apps, w);
    let cell_w = grid_cell_width(apps, w);
    let (r, c) = (row / columns, row % columns);
    Rect::new(
        apps.x + c as i32 * cell_w,
        apps.y + r as i32 * h - d.menu.app_scroll as i32,
        cell_w,
        h,
    )
}

/// The rectangle of a favorite's tile in the tiles area.
///
/// # Arguments
/// * `tiles` - The tiles area.
/// * `index` - The favorite's position.
fn tile_rect(tiles: Rect, index: usize) -> Rect {
    let columns = ((tiles.w + TILE_GAP) / (TILE + TILE_GAP)).max(1) as usize;
    let (r, c) = (index / columns, index % columns);
    Rect::new(
        tiles.x + c as i32 * (TILE + TILE_GAP),
        tiles.y + r as i32 * (TILE + TILE_GAP),
        TILE,
        TILE,
    )
}

/// Height of the application area's content, for scrolling.
fn apps_content_height(d: &Desktop, apps: Rect) -> f64 {
    match arrangement(d) {
        Arrangement::Grid { w, h, .. } => {
            let rows = d.menu.items.len().div_ceil(grid_columns(apps, w));
            rows as f64 * h as f64
        }
        Arrangement::Rows => d.menu.items.len() as f64 * ROW as f64,
    }
}

/// Whether the application list is the favorites, in their order, so its cells can be
/// reordered.
fn favorites_shown(d: &Desktop) -> bool {
    d.menu.query.trim().is_empty() && d.menu.section == Section::Favorites
}

/// Holds a favorite on its press, to launch on the release or drag to a new place.
fn hold_favorite(d: &mut Desktop, from: usize, x: f64, y: f64) {
    d.menu.favorite_drag = Some(FavoriteDrag {
        from,
        start: (x, y),
        at: (x, y),
        dragging: false,
    });
}

/// The place among the favorites a point drops a dragged one on: the cell or tile under it, a
/// point past the last one counting as the last place.
fn favorite_drop_at(d: &Desktop, layout: &Layout, x: f64, y: f64) -> Option<usize> {
    let count = favorite_entries(d).len();
    if count == 0 {
        return None;
    }
    if layout.tiles.w > 0 {
        if !layout.tiles.contains(x, y) {
            return None;
        }
        return Some(
            (0..count)
                .find(|&i| tile_rect(layout.tiles, i).contains(x, y))
                .unwrap_or(count - 1),
        );
    }
    if !favorites_shown(d) || !layout.apps.contains(x, y) {
        return None;
    }
    let Arrangement::Grid { w, h, .. } = arrangement(d) else {
        let line = ((y - layout.apps.y as f64 + d.menu.app_scroll) / ROW as f64).floor();
        return Some((line.max(0.0) as usize).min(count - 1));
    };
    let columns = grid_columns(layout.apps, w);
    let column = ((x - layout.apps.x as f64) / grid_cell_width(layout.apps, w) as f64).floor();
    let line = ((y - layout.apps.y as f64 + d.menu.app_scroll) / h as f64).floor();
    let place = line.max(0.0) as usize * columns + (column.max(0.0) as usize).min(columns - 1);
    Some(place.min(count - 1))
}

/// The rectangle of the favorite at a place, as the current layout draws it.
fn favorite_rect(d: &Desktop, layout: &Layout, place: usize) -> Option<Rect> {
    if layout.tiles.w > 0 {
        return Some(tile_rect(layout.tiles, place));
    }
    match arrangement(d) {
        Arrangement::Grid { w, h, .. } => Some(grid_cell(d, layout.apps, place, w, h)),
        Arrangement::Rows => Some(Rect::new(
            layout.apps.x,
            layout.apps.y + place as i32 * ROW - d.menu.app_scroll as i32,
            content_width(layout.apps),
            ROW,
        )),
    }
}

/// Follows the pointer with a held favorite, which drags once it has moved far enough.
///
/// # Arguments
/// * `d` - The desktop.
/// * `x` - Pointer X.
/// * `y` - Pointer Y.
pub fn favorite_motion(d: &mut Desktop, x: f64, y: f64) {
    let Some(drag) = d.menu.favorite_drag.as_mut() else {
        return;
    };
    drag.at = (x, y);
    let (dx, dy) = (x - drag.start.0, y - drag.start.1);
    if !drag.dragging && dx.hypot(dy) >= FAVORITE_DRAG_THRESHOLD {
        drag.dragging = true;
    }
    if drag.dragging {
        d.redraw_panel();
    }
}

/// Ends a held favorite on the release: a favorite that did not drag launches, one dragged
/// onto another place moves there.
///
/// # Arguments
/// * `d` - The desktop.
/// * `x` - Pointer X.
/// * `y` - Pointer Y.
pub fn favorite_release(d: &mut Desktop, x: f64, y: f64) {
    let Some(drag) = d.menu.favorite_drag.take() else {
        return;
    };
    if !drag.dragging {
        if let Some(index) = favorite_entries(d).get(drag.from).copied() {
            d.launch_app(index);
        }
        return;
    }
    let layout = layout(d);
    if let Some(to) = favorite_drop_at(d, &layout, x, y)
        && to != drag.from
    {
        move_favorite(d, drag.from, to);
    }
    let hover = hit(d, x, y).unwrap_or(Hover::None);
    d.set_hover(hover);
    d.redraw_panel();
}

/// Moves the favorite at one place among those shown to another and writes the order, the
/// favorites not installed keeping their places in the setting.
fn move_favorite(d: &mut Desktop, from: usize, to: usize) {
    let shown: Vec<String> = favorite_entries(d)
        .into_iter()
        .map(|index| d.apps.entries[index].id.clone())
        .collect();
    if from >= shown.len() || to >= shown.len() {
        return;
    }
    let mut order = shown.clone();
    let moved = order.remove(from);
    order.insert(to, moved);
    let list = reorder_setting(&d.config.favorites, &shown, order);
    d.write_config(&[("favorites", list.join(", "))]);
    d.menu.invalidate();
}

/// The setting's list with the entries shown put back into the slots they held in it, in their
/// new order, every other entry where it was.
fn reorder_setting(setting: &[String], shown: &[String], order: Vec<String>) -> Vec<String> {
    let mut next = order.into_iter();
    setting
        .iter()
        .map(|id| {
            if shown.contains(id) {
                next.next().unwrap_or_else(|| id.clone())
            } else {
                id.clone()
            }
        })
        .collect()
}

/// Draws a dragged favorite: the place it would drop on outlined in the accent, and its icon
/// under the pointer.
fn draw_favorite_drag(d: &mut Desktop, canvas: &mut Canvas<'_>, layout: &Layout) {
    let Some(drag) = d.menu.favorite_drag.filter(|drag| drag.dragging) else {
        return;
    };
    if let Some(target) = favorite_drop_at(d, layout, drag.at.0, drag.at.1)
        && let Some(rect) = favorite_rect(d, layout, target)
    {
        canvas.stroke_rounded(rect.inflate(-2), 6.0, 3.0, ui::accent());
    }
    let Some(index) = favorite_entries(d).get(drag.from).copied() else {
        return;
    };
    let size = if layout.tiles.w > 0 {
        TILE_ICON
    } else {
        FAVORITE_ICON
    };
    let rect = Rect::new(
        drag.at.0 as i32 - size / 2,
        drag.at.1 as i32 - size / 2,
        size,
        size,
    );
    if let Some(icon) = d.app_icon_sized(index, size) {
        canvas.draw_image_fit(&icon, rect);
    }
}

/// The rectangle a column scrolls in.
fn column_rect(layout: &Layout, column: ScrollColumn) -> Rect {
    match column {
        ScrollColumn::Categories => layout.categories,
        ScrollColumn::Apps => layout.apps,
    }
}

/// How far a column's content reaches below its top, and how far it is scrolled.
fn column_extent(d: &Desktop, layout: &Layout, column: ScrollColumn) -> (f64, f64) {
    match column {
        ScrollColumn::Categories => (
            category_rows(d).len() as f64 * CATEGORY_ROW as f64,
            d.menu.category_scroll,
        ),
        ScrollColumn::Apps => (apps_content_height(d, layout.apps), d.menu.app_scroll),
    }
}

/// A column's scrollbar track and thumb, or `None` while its content fits.
fn scrollbar(d: &Desktop, layout: &Layout, column: ScrollColumn) -> Option<(Rect, Rect)> {
    let rect = column_rect(layout, column);
    if rect.w <= 0 || rect.h <= 0 {
        return None;
    }
    let (content, scroll) = column_extent(d, layout, column);
    let max = content - rect.h as f64;
    if max <= 0.0 {
        return None;
    }
    let track = Rect::new(rect.right() - SCROLLBAR_W, rect.y, SCROLLBAR_W, rect.h);
    let thumb_h =
        ((rect.h as f64 * rect.h as f64 / content) as i32).clamp(SCROLLBAR_MIN_THUMB.min(rect.h), rect.h);
    let travel = (track.h - thumb_h) as f64;
    let thumb_y = track.y + (travel * (scroll / max).clamp(0.0, 1.0)).round() as i32;
    Some((track, Rect::new(track.x, thumb_y, SCROLLBAR_W, thumb_h)))
}

/// Follows the pointer with the dragged scrollbar's thumb, scrolling its column to match.
///
/// # Arguments
/// * `d` - The desktop.
/// * `y` - Pointer Y.
pub fn scroll_drag(d: &mut Desktop, y: f64) {
    let Some((column, offset)) = d.menu.scroll_drag else {
        return;
    };
    let layout = layout(d);
    let Some((track, thumb)) = scrollbar(d, &layout, column) else {
        return;
    };
    let rect = column_rect(&layout, column);
    let max = column_extent(d, &layout, column).0 - rect.h as f64;
    let travel = (track.h - thumb.h).max(1) as f64;
    let scroll = ((y - offset - track.y as f64) / travel).clamp(0.0, 1.0) * max;
    match column {
        ScrollColumn::Categories => d.menu.category_scroll = scroll,
        ScrollColumn::Apps => d.menu.app_scroll = scroll,
    }
    d.redraw_panel();
}

/// Draws the scrollbar of every column whose content overflows: the thumb over a faint track,
/// brighter while hovered or dragged.
fn draw_scrollbars(d: &mut Desktop, canvas: &mut Canvas<'_>, layout: &Layout) {
    for column in [ScrollColumn::Categories, ScrollColumn::Apps] {
        let Some((track, thumb)) = scrollbar(d, layout, column) else {
            continue;
        };
        let active =
            d.hover == Hover::MenuScrollbar(column) || d.menu.scroll_drag.is_some_and(|(c, _)| c == column);
        let radius = SCROLLBAR_W as f32 / 2.0;
        canvas.fill_rounded(track, radius, ui::text_alpha(0x14));
        canvas.fill_rounded(thumb, radius, ui::text_alpha(if active { 0xa0 } else { 0x60 }));
    }
}

/// The rows of an application's right-click menu: each toggles the
/// application's place in a list setting.
///
/// # Arguments
/// * `d` - The desktop.
/// * `index` - The application's entry index.
fn context_rows(d: &Desktop, index: usize) -> Vec<(&'static str, &'static str, String)> {
    let Some(entry) = d.apps.entries.get(index) else {
        return Vec::new();
    };
    let id = entry.id.clone();
    let favorite = d.config.favorites.contains(&id);
    let launcher = d.config.launchers.contains(&id);
    let on_desktop = entry
        .path
        .file_name()
        .is_some_and(|name| entries::desktop_dir().join(name).exists());
    let mut rows = vec![
        (
            if favorite {
                tr("Unpin from favorites")
            } else {
                tr("Pin to favorites")
            },
            "favorites",
            id.clone(),
        ),
        (
            if launcher {
                tr("Remove from quick launch")
            } else {
                tr("Add to quick launch")
            },
            "launchers",
            id.clone(),
        ),
    ];
    if !on_desktop {
        rows.push((tr("Add to desktop"), "desktop", id));
    }
    rows
}

/// The rectangle of the open right-click menu, kept inside the panel surface.
fn context_rect(d: &Desktop) -> Option<Rect> {
    let (index, x, y) = d.menu.context?;
    let height = context_rows(d, index).len() as i32 * CONTEXT_ROW + 12;
    let x = x
        .min(d.panel.width - CONTEXT_WIDTH - ui::POPUP_MARGIN)
        .max(ui::POPUP_MARGIN);
    let y = if y + height > d.panel.height - ui::POPUP_MARGIN {
        y - height
    } else {
        y
    };
    Some(Rect::new(x, y.max(ui::POPUP_MARGIN), CONTEXT_WIDTH, height))
}

/// The hover target under a point in the menu, or `None` outside of it.
///
/// # Arguments
/// * `d` - The desktop.
/// * `x` - Pointer X.
/// * `y` - Pointer Y.
pub fn hit(d: &mut Desktop, x: f64, y: f64) -> Option<Hover> {
    ensure_items(d);
    let layout = layout(d);
    if let Some(rect) = context_rect(d)
        && let Some((index, _, _)) = d.menu.context
    {
        if rect.contains(x, y) {
            let row = ((y - rect.y as f64 - 6.0) / CONTEXT_ROW as f64).floor();
            if row >= 0.0 && (row as usize) < context_rows(d, index).len() {
                return Some(Hover::MenuContextItem(row as usize));
            }
        }
        return Some(Hover::None);
    }
    if !layout.frame.contains(x, y) {
        return None;
    }
    if layout.settings.contains(x, y) {
        return Some(Hover::MenuSettings);
    }
    for column in [ScrollColumn::Categories, ScrollColumn::Apps] {
        if let Some((track, _)) = scrollbar(d, &layout, column)
            && Rect::new(track.right() - SCROLLBAR_HIT, track.y, SCROLLBAR_HIT, track.h).contains(x, y)
        {
            return Some(Hover::MenuScrollbar(column));
        }
    }
    if layout.categories.w > 0 && layout.categories.contains(x, y) {
        let row = ((y - layout.categories.y as f64 + d.menu.category_scroll) / CATEGORY_ROW as f64).floor();
        if row >= 0.0
            && let Some(CategoryRow::Section(_)) = category_rows(d).get(row as usize)
        {
            return Some(Hover::MenuCategory(row as usize));
        }
        return Some(Hover::None);
    }
    if layout.apps.contains(x, y) {
        let row = match arrangement(d) {
            Arrangement::Grid { w, h, .. } => {
                let columns = grid_columns(layout.apps, w);
                let column = ((x - layout.apps.x as f64) / grid_cell_width(layout.apps, w) as f64).floor();
                let line = ((y - layout.apps.y as f64 + d.menu.app_scroll) / h as f64).floor();
                if column < 0.0 || column as usize >= columns || line < 0.0 {
                    return Some(Hover::None);
                }
                line as usize * columns + column as usize
            }
            Arrangement::Rows => {
                let line = ((y - layout.apps.y as f64 + d.menu.app_scroll) / ROW as f64).floor();
                if line < 0.0 {
                    return Some(Hover::None);
                }
                line as usize
            }
        };
        if row < d.menu.items.len() {
            return Some(Hover::MenuApp(row));
        }
        return Some(Hover::None);
    }
    if layout.tiles.w > 0 && layout.tiles.contains(x, y) {
        let count = favorite_entries(d).len();
        for index in 0..count {
            if tile_rect(layout.tiles, index).contains(x, y) {
                return Some(Hover::MenuTile(index));
            }
        }
    }
    Some(Hover::None)
}

/// The entry index a hover target names, if it is an application.
fn hovered_entry(d: &Desktop, hover: Hover) -> Option<usize> {
    match hover {
        Hover::MenuApp(row) => d.menu.items.get(row).copied(),
        Hover::MenuTile(index) => favorite_entries(d).get(index).copied(),
        _ => None,
    }
}

/// Handles a click; returns whether it landed inside the menu.
///
/// # Arguments
/// * `d` - The desktop.
/// * `x` - Pointer X.
/// * `y` - Pointer Y.
/// * `button` - The button.
pub fn click(d: &mut Desktop, x: f64, y: f64, button: Button) -> bool {
    let Some(hover) = hit(d, x, y) else {
        return false;
    };
    if let Some((index, _, _)) = d.menu.context.take() {
        if button == Button::Left
            && let Hover::MenuContextItem(item) = hover
            && let Some((_, key, id)) = context_rows(d, index).get(item).cloned()
        {
            if key == "desktop" {
                d.add_to_desktop(&id);
            } else {
                d.toggle_list_entry(key, &id);
            }
        }
        let hover = hit(d, x, y).unwrap_or(Hover::None);
        d.set_hover(hover);
        d.redraw_panel();
        return true;
    }
    if button == Button::Right {
        if let Some(index) = hovered_entry(d, hover) {
            d.menu.context = Some((index, x as i32, y as i32));
            d.set_hover(Hover::None);
            d.redraw_panel();
        }
        return true;
    }
    if button != Button::Left {
        return true;
    }
    match hover {
        Hover::MenuApp(row) if favorites_shown(d) => hold_favorite(d, row, x, y),
        Hover::MenuTile(position) => hold_favorite(d, position, x, y),
        Hover::MenuApp(_) => {
            if let Some(index) = hovered_entry(d, hover) {
                d.launch_app(index);
            }
        }
        Hover::MenuCategory(row) => {
            if let Some(CategoryRow::Section(section)) = category_rows(d).get(row).copied()
                && d.menu.section != section
            {
                d.menu.section = section;
                d.menu.app_scroll = 0.0;
                d.menu.selected = None;
                d.menu.items_valid = false;
                ensure_items(d);
            }
            d.redraw_panel();
        }
        Hover::MenuScrollbar(column) => {
            let layout = layout(d);
            if let Some((_, thumb)) = scrollbar(d, &layout, column) {
                let offset = if thumb.contains(x, y) {
                    y - thumb.y as f64
                } else {
                    thumb.h as f64 / 2.0
                };
                d.menu.scroll_drag = Some((column, offset));
                scroll_drag(d, y);
            }
        }
        Hover::MenuSettings => {
            d.close_popup();
            d.open_settings(None);
        }
        _ => {}
    }
    true
}

/// Handles wheel scrolling; returns whether it landed inside the menu.
///
/// # Arguments
/// * `d` - The desktop.
/// * `x` - Pointer X.
/// * `y` - Pointer Y.
/// * `delta` - Scroll amount, positive downwards.
pub fn scroll(d: &mut Desktop, x: f64, y: f64, delta: f64) -> bool {
    ensure_items(d);
    let layout = layout(d);
    if !layout.frame.contains(x, y) {
        return false;
    }
    let step = delta.signum() * ui::SCROLL_STEP;
    if layout.categories.w > 0 && layout.categories.contains(x, y) {
        let content = category_rows(d).len() as f64 * CATEGORY_ROW as f64;
        let max = (content - layout.categories.h as f64).max(0.0);
        d.menu.category_scroll = (d.menu.category_scroll + step).clamp(0.0, max);
        d.redraw_panel();
    } else if layout.apps.contains(x, y) {
        let content = apps_content_height(d, layout.apps);
        let max = (content - layout.apps.h as f64).max(0.0);
        d.menu.app_scroll = (d.menu.app_scroll + step).clamp(0.0, max);
        if let Some(hover) = hit(d, x, y) {
            d.set_hover(hover);
        }
        d.redraw_panel();
    }
    true
}

/// Handles a key press while the menu is open.  In the grid the arrows
/// move the selection by cell, elsewhere up and down move it by row and
/// left and right change the category.
///
/// # Arguments
/// * `d` - The desktop.
/// * `key` - The key.
pub fn key(d: &mut Desktop, key: Key) {
    ensure_items(d);
    let count = d.menu.items.len();
    let columns = match arrangement(d) {
        Arrangement::Grid { w, .. } => grid_columns(layout(d).apps, w) as i32,
        Arrangement::Rows => 1,
    };
    match key {
        Key::Char(c) => {
            d.menu.context = None;
            d.menu.query.push(c);
            d.menu.items_valid = false;
            d.menu.app_scroll = 0.0;
            ensure_items(d);
            d.menu.selected = if d.menu.items.is_empty() { None } else { Some(0) };
        }
        Key::Backspace => {
            if d.menu.query.pop().is_some() {
                d.menu.items_valid = false;
                d.menu.app_scroll = 0.0;
                ensure_items(d);
                d.menu.selected = if d.menu.query.is_empty() || d.menu.items.is_empty() {
                    None
                } else {
                    Some(0)
                };
            }
        }
        Key::Enter => {
            let row = d.menu.selected.or(if count > 0 { Some(0) } else { None });
            if let Some(index) = row.and_then(|r| d.menu.items.get(r).copied()) {
                d.launch_app(index);
                return;
            }
        }
        Key::Down => move_selection(d, columns),
        Key::Up => move_selection(d, -columns),
        Key::PageDown => move_selection(d, 10 * columns),
        Key::PageUp => move_selection(d, -10 * columns),
        Key::Home => {
            if count > 0 {
                d.menu.selected = Some(0);
                scroll_to_selection(d);
            }
        }
        Key::End => {
            if count > 0 {
                d.menu.selected = Some(count - 1);
                scroll_to_selection(d);
            }
        }
        Key::Left | Key::Right if columns > 1 => {
            move_selection(d, if key == Key::Right { 1 } else { -1 });
        }
        Key::Left | Key::Right => {
            if d.menu.query.trim().is_empty() && d.config.menu_layout != MenuLayout::Tiles {
                let rows = category_rows(d);
                let sections: Vec<Section> = rows
                    .iter()
                    .filter_map(|r| {
                        if let CategoryRow::Section(s) = r {
                            Some(*s)
                        } else {
                            None
                        }
                    })
                    .collect();
                if let Some(pos) = sections.iter().position(|s| *s == d.menu.section) {
                    let next = if key == Key::Right {
                        (pos + 1) % sections.len()
                    } else {
                        (pos + sections.len() - 1) % sections.len()
                    };
                    d.menu.section = sections[next];
                    d.menu.items_valid = false;
                    d.menu.selected = None;
                    d.menu.app_scroll = 0.0;
                    let row_index = rows
                        .iter()
                        .position(|r| *r == CategoryRow::Section(sections[next]))
                        .unwrap_or(0);
                    let layout = layout(d);
                    let row_top = row_index as f64 * CATEGORY_ROW as f64;
                    if row_top < d.menu.category_scroll {
                        d.menu.category_scroll = row_top;
                    } else if row_top + CATEGORY_ROW as f64
                        > d.menu.category_scroll + layout.categories.h as f64
                    {
                        d.menu.category_scroll = row_top + CATEGORY_ROW as f64 - layout.categories.h as f64;
                    }
                }
            }
        }
        Key::Tab | Key::Delete | Key::Escape | Key::Super | Key::Function(_) => {}
    }
    d.redraw_panel();
}

/// Moves the keyboard selection by a number of items.
fn move_selection(d: &mut Desktop, by: i32) {
    let count = d.menu.items.len() as i32;
    if count == 0 {
        d.menu.selected = None;
        return;
    }
    let current = d.menu.selected.map(|s| s as i32).unwrap_or(-1);
    let next = (current + by).clamp(0, count - 1);
    d.menu.selected = Some(next as usize);
    scroll_to_selection(d);
}

/// Scrolls the application area so the selected item is visible.
fn scroll_to_selection(d: &mut Desktop) {
    let Some(selected) = d.menu.selected else {
        return;
    };
    let layout = layout(d);
    let (top, height) = match arrangement(d) {
        Arrangement::Grid { w, h, .. } => {
            let row = selected / grid_columns(layout.apps, w);
            (row as f64 * h as f64, h as f64)
        }
        Arrangement::Rows => (selected as f64 * ROW as f64, ROW as f64),
    };
    if top < d.menu.app_scroll {
        d.menu.app_scroll = top;
    } else if top + height > d.menu.app_scroll + layout.apps.h as f64 {
        d.menu.app_scroll = top + height - layout.apps.h as f64;
    }
}

/// Draws the menu and returns its rectangle.
///
/// # Arguments
/// * `d` - The desktop.
/// * `canvas` - The panel canvas.
pub fn draw(d: &mut Desktop, canvas: &mut Canvas<'_>) -> Rect {
    ensure_items(d);
    let layout = layout(d);
    ui::draw_popup_frame(canvas, layout.frame);

    canvas.fill_rounded(layout.search, 6.0, ui::field_bg());
    if let Some(glass) = ui::search_path(layout.search.x + 16, layout.search.y + layout.search.h / 2) {
        canvas.stroke_path(&glass, 1.6, ui::text_dim());
    }
    let text_rect = Rect::new(
        layout.search.x + 32,
        layout.search.y,
        layout.search.w - 40,
        layout.search.h,
    );
    if d.menu.query.is_empty() {
        ui::label(
            canvas,
            &mut d.fonts,
            tr("Search applications"),
            text_rect,
            ui::FONT_SIZE,
            false,
            Align::Left,
            ui::text_dim(),
        );
    } else {
        let query = d.menu.query.clone();
        ui::label(
            canvas,
            &mut d.fonts,
            &query,
            text_rect,
            ui::FONT_SIZE,
            false,
            Align::Left,
            ui::text(),
        );
    }
    if d.popup == Popup::Menu {
        let query = d.menu.query.clone();
        let caret_x = text_rect.x
            + ui::text_width(&mut d.fonts, &query, ui::FONT_SIZE, false, d.panel.scale).min(text_rect.w - 2);
        canvas.fill_rect(
            Rect::new(caret_x + 1, text_rect.y + 9, 1, text_rect.h - 18),
            ui::text(),
        );
    }

    draw_settings_button(d, canvas, layout.settings);
    if layout.categories.w > 0 {
        draw_categories(d, canvas, &layout);
    }
    canvas.clip(layout.apps);
    if d.menu.items.is_empty() {
        let message = if d.menu.query.trim().is_empty() {
            tr("No applications")
        } else {
            tr("No matches")
        };
        let rect = Rect::new(layout.apps.x, layout.apps.y, layout.apps.w, ROW);
        ui::label(
            canvas,
            &mut d.fonts,
            message,
            rect,
            ui::FONT_SIZE,
            false,
            Align::Left,
            ui::text_dim(),
        );
    }
    match arrangement(d) {
        Arrangement::Grid { w, h, icon } => draw_grid(d, canvas, &layout, w, h, icon),
        Arrangement::Rows => draw_rows(d, canvas, &layout),
    }
    canvas.reset_clip();
    draw_scrollbars(d, canvas, &layout);
    if layout.tiles.w > 0 {
        draw_tiles(d, canvas, &layout);
    }
    draw_favorite_drag(d, canvas, &layout);

    canvas.fill_rect(
        Rect::new(
            layout.footer.x + PAD,
            layout.footer.y,
            layout.footer.w - 2 * PAD,
            1,
        ),
        ui::separator(),
    );
    let identity = d.menu.identity.clone();
    let footer_text = Rect::new(
        layout.footer.x + PAD + 2,
        layout.footer.y + 2,
        layout.footer.w - 2 * PAD,
        layout.footer.h - 6,
    );
    ui::label(
        canvas,
        &mut d.fonts,
        &identity,
        footer_text,
        ui::FONT_SMALL,
        false,
        Align::Left,
        ui::text_dim(),
    );
    let count_text = trf("{n} apps", &[("n", &d.apps.entries.len().to_string())]);
    ui::label(
        canvas,
        &mut d.fonts,
        &count_text,
        footer_text,
        ui::FONT_SMALL,
        false,
        Align::Right,
        ui::text_dim(),
    );

    if let Some((index, _, _)) = d.menu.context
        && let Some(rect) = context_rect(d)
    {
        canvas.fill_rounded(
            rect.inflate(1),
            ui::popup_radius(),
            crate::render::rgba(0, 0, 0, 0x50),
        );
        ui::draw_popup_frame(canvas, rect);
        for (i, (label, _, _)) in context_rows(d, index).into_iter().enumerate() {
            let item = Rect::new(
                rect.x + 6,
                rect.y + 6 + i as i32 * CONTEXT_ROW,
                rect.w - 12,
                CONTEXT_ROW,
            );
            if d.hover == Hover::MenuContextItem(i) {
                canvas.fill_rounded(item, 5.0, ui::hover_bg());
            }
            ui::label(
                canvas,
                &mut d.fonts,
                label,
                Rect::new(item.x + 10, item.y, item.w - 20, item.h),
                ui::FONT_SIZE,
                false,
                Align::Left,
                ui::text(),
            );
        }
    }

    layout.frame
}

/// Draws the Settings button: the settings icon and its label on a field-colored button, lit
/// while hovered.
fn draw_settings_button(d: &mut Desktop, canvas: &mut Canvas<'_>, rect: Rect) {
    canvas.fill_rounded(rect, 6.0, ui::field_bg());
    if d.hover == Hover::MenuSettings {
        canvas.fill_rounded(rect, 6.0, ui::hover_bg());
    }
    let size = CATEGORY_ICON;
    let icon_rect = Rect::new(rect.x + 10, rect.y + (rect.h - size) / 2, size, size);
    let scale = d.panel.scale.max(1);
    if let Some(icon) = d.icons.get("preferences-system", (size * scale) as u32) {
        canvas.draw_image_fit(&icon, icon_rect);
    }
    let text = Rect::new(
        icon_rect.right() + 8,
        rect.y,
        rect.right() - icon_rect.right() - 16,
        rect.h,
    );
    ui::label(
        canvas,
        &mut d.fonts,
        tr("Settings"),
        text,
        ui::FONT_SIZE,
        false,
        Align::Left,
        ui::text(),
    );
}

/// Draws the category column.
fn draw_categories(d: &mut Desktop, canvas: &mut Canvas<'_>, layout: &Layout) {
    canvas.clip(layout.categories);
    let rows = category_rows(d);
    for (row, item) in rows.iter().enumerate() {
        let y = layout.categories.y + row as i32 * CATEGORY_ROW - d.menu.category_scroll as i32;
        if y + CATEGORY_ROW < layout.categories.y || y > layout.categories.bottom() {
            continue;
        }
        match item {
            CategoryRow::Separator => {
                canvas.fill_rect(
                    Rect::new(
                        layout.categories.x + 8,
                        y + CATEGORY_ROW / 2,
                        content_width(layout.categories) - 16,
                        1,
                    ),
                    ui::separator(),
                );
            }
            CategoryRow::Section(section) => {
                let rect = Rect::new(
                    layout.categories.x,
                    y,
                    content_width(layout.categories),
                    CATEGORY_ROW,
                );
                let current = d.menu.section == *section;
                let hovered = d.hover == Hover::MenuCategory(row);
                if current {
                    canvas.fill_rounded(rect, 5.0, ui::active_bg());
                } else if hovered {
                    canvas.fill_rounded(rect, 5.0, ui::hover_bg());
                }
                let icon_rect = Rect::new(
                    rect.x + 8,
                    rect.y + (CATEGORY_ROW - CATEGORY_ICON) / 2,
                    CATEGORY_ICON,
                    CATEGORY_ICON,
                );
                let scale = d.panel.scale.max(1);
                if let Some(icon) = d
                    .icons
                    .get(section_icon(d, *section), (CATEGORY_ICON * scale) as u32)
                {
                    canvas.draw_image_fit(&icon, icon_rect);
                }
                let name = section_name(d, *section);
                let label_rect = Rect::new(
                    icon_rect.right() + 10,
                    rect.y,
                    rect.right() - icon_rect.right() - 16,
                    rect.h,
                );
                ui::label(
                    canvas,
                    &mut d.fonts,
                    &name,
                    label_rect,
                    ui::FONT_SIZE,
                    current,
                    Align::Left,
                    ui::text(),
                );
            }
        }
    }
    canvas.reset_clip();
}

/// Draws the applications as rows, the canvas clipped to their area.
fn draw_rows(d: &mut Desktop, canvas: &mut Canvas<'_>, layout: &Layout) {
    let items = d.menu.items.clone();
    for (row, index) in items.iter().enumerate() {
        let y = layout.apps.y + row as i32 * ROW - d.menu.app_scroll as i32;
        if y + ROW < layout.apps.y || y > layout.apps.bottom() {
            continue;
        }
        let rect = Rect::new(layout.apps.x, y, content_width(layout.apps), ROW);
        let selected = d.menu.selected == Some(row);
        let hovered = d.hover == Hover::MenuApp(row);
        if selected {
            canvas.fill_rounded(rect, 5.0, ui::active_bg());
        } else if hovered {
            canvas.fill_rounded(rect, 5.0, ui::hover_bg());
        }
        let icon_rect = Rect::new(rect.x + 8, rect.y + (ROW - ROW_ICON) / 2, ROW_ICON, ROW_ICON);
        if let Some(icon) = d.app_icon_sized(*index, ROW_ICON) {
            canvas.draw_image_fit(&icon, icon_rect);
        } else {
            canvas.fill_rounded(icon_rect.inflate(-4), 4.0, ui::field_bg());
        }
        let entry = &d.apps.entries[*index];
        let name = entry.name.clone();
        let description = entry
            .comment
            .clone()
            .or_else(|| entry.generic_name.clone())
            .filter(|text| !text.trim().is_empty() && *text != name);
        let text_x = icon_rect.right() + 10;
        let text_w = rect.right() - text_x - 8;
        let name_rect = match description {
            Some(_) => Rect::new(text_x, rect.y + 4, text_w, 20),
            None => Rect::new(text_x, rect.y, text_w, rect.h),
        };
        ui::label(
            canvas,
            &mut d.fonts,
            &name,
            name_rect,
            ui::FONT_SIZE,
            false,
            Align::Left,
            ui::text(),
        );
        if let Some(description) = description {
            ui::label(
                canvas,
                &mut d.fonts,
                &description,
                Rect::new(text_x, rect.y + 22, text_w, 18),
                ui::FONT_SMALL,
                false,
                Align::Left,
                ui::text_dim(),
            );
        }
    }
}

/// Draws the applications as a grid of icons with their names beneath,
/// up to two lines, the canvas clipped to their area.
///
/// # Arguments
/// * `w` - The cell width.
/// * `h` - The cell height.
/// * `icon_size` - The icon size.
fn draw_grid(d: &mut Desktop, canvas: &mut Canvas<'_>, layout: &Layout, w: i32, h: i32, icon_size: i32) {
    let items = d.menu.items.clone();
    let scale = canvas.scale() as f32;
    for (row, index) in items.iter().enumerate() {
        let cell = grid_cell(d, layout.apps, row, w, h);
        if cell.bottom() < layout.apps.y || cell.y > layout.apps.bottom() {
            continue;
        }
        let selected = d.menu.selected == Some(row);
        let hovered = d.hover == Hover::MenuApp(row);
        if selected {
            canvas.fill_rounded(cell.inflate(-2), 6.0, ui::active_bg());
        } else if hovered {
            canvas.fill_rounded(cell.inflate(-2), 6.0, ui::hover_bg());
        }
        let icon_rect = Rect::new(
            cell.x + (cell.w - icon_size) / 2,
            cell.y + 8,
            icon_size,
            icon_size,
        );
        if let Some(icon) = d.app_icon_sized(*index, icon_size) {
            canvas.draw_image_fit(&icon, icon_rect);
        } else {
            canvas.fill_rounded(icon_rect.inflate(-4), 4.0, ui::field_bg());
        }
        let name = d.apps.entries[*index].name.clone();
        let font = if icon_size >= FAVORITE_ICON {
            ui::FONT_SIZE
        } else {
            ui::FONT_SMALL
        };
        let (line1, line2) =
            ui::wrap_two_lines(&mut d.fonts, &name, font * scale, (cell.w - 8) as f32 * scale);
        let line_h = (font + 4.0) as i32;
        let top = icon_rect.bottom() + 4;
        canvas.draw_text(
            &mut d.fonts,
            &line1,
            cell.x,
            top + line_h / 2,
            cell.w,
            Align::Center,
            ui::text(),
        );
        if let Some(line2) = line2 {
            canvas.draw_text(
                &mut d.fonts,
                &line2,
                cell.x,
                top + line_h + line_h / 2,
                cell.w,
                Align::Center,
                ui::text(),
            );
        }
    }
}

/// Draws the favorites as tiles: squares in the accent with the icon and
/// the name on each.
fn draw_tiles(d: &mut Desktop, canvas: &mut Canvas<'_>, layout: &Layout) {
    canvas.clip(layout.tiles);
    let favorites = favorite_entries(d);
    for (position, index) in favorites.iter().enumerate() {
        let tile = tile_rect(layout.tiles, position);
        if tile.y > layout.tiles.bottom() {
            break;
        }
        let hovered = d.hover == Hover::MenuTile(position);
        let radius = (ui::popup_radius() / 2.0).min(6.0);
        canvas.fill_rounded(tile, radius, ui::accent_alpha(if hovered { 0xff } else { 0xd8 }));
        if hovered {
            canvas.stroke_rounded(tile, radius, 1.5, ui::text_alpha(0x90));
        }
        let icon_rect = Rect::new(
            tile.x + (tile.w - TILE_ICON) / 2,
            tile.y + 12,
            TILE_ICON,
            TILE_ICON,
        );
        if let Some(icon) = d.app_icon_sized(*index, TILE_ICON) {
            canvas.draw_image_fit(&icon, icon_rect);
        }
        let name = d.apps.entries[*index].name.clone();
        ui::label(
            canvas,
            &mut d.fonts,
            &name,
            Rect::new(tile.x + 4, tile.bottom() - 26, tile.w - 8, 22),
            ui::FONT_SMALL,
            false,
            Align::Center,
            ui::on_accent(),
        );
    }
    canvas.reset_clip();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The application column of a full-width menu holds four favorites and five application
    /// cells beside its scrollbar's gutter, the cells spanning the content width without
    /// reaching the gutter.
    #[test]
    fn the_favorites_grid_fits_four_columns_beside_the_scrollbar() {
        let apps = Rect::new(0, 0, WIDTH - 3 * PAD - CATEGORY_WIDTH, 300);
        assert_eq!(grid_columns(apps, FAVORITE_W), 4);
        assert_eq!(grid_columns(apps, CELL_W), 5);
        for cell_w in [FAVORITE_W, CELL_W] {
            let span = grid_cell_width(apps, cell_w) * grid_columns(apps, cell_w) as i32;
            assert!(span <= apps.w - SCROLL_GUTTER && span > apps.w - SCROLL_GUTTER - 5);
        }
    }

    /// A reordered favorite moves among the installed ones while an entry not installed keeps
    /// its slot in the setting.
    #[test]
    fn a_moved_favorite_keeps_the_uninstalled_ones_in_place() {
        let ids = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let setting = ids(&["a", "gone", "b", "c"]);
        let shown = ids(&["a", "b", "c"]);
        let order = ids(&["c", "a", "b"]);
        assert_eq!(
            reorder_setting(&setting, &shown, order),
            ids(&["c", "gone", "a", "b"])
        );
    }

    #[test]
    fn grid_and_tiles_place_cells_by_width() {
        let apps = Rect::new(10, 20, 300, 300);
        assert_eq!(grid_columns(apps, CELL_W), 4);
        assert_eq!(grid_columns(Rect::new(0, 0, 10, 10), CELL_W), 1);
        let tiles = Rect::new(230, 20, 260, 300);
        assert_eq!(tile_rect(tiles, 0), Rect::new(230, 20, TILE, TILE));
        assert_eq!(tile_rect(tiles, 2).x, 230 + 2 * (TILE + TILE_GAP));
        assert_eq!(
            tile_rect(tiles, 3),
            Rect::new(230, 20 + TILE + TILE_GAP, TILE, TILE)
        );
        assert!(tile_rect(tiles, 2).right() <= tiles.right());
    }
}
