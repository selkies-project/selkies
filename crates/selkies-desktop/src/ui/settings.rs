//! The bar's right-click menu and the settings window it opens: a column of
//! pages on the left and the page's settings on the right, in captioned
//! cards with one setting per row, its name on the left and its control on
//! the right.  The Panel page holds the bar and the start menu, the Files
//! page the file manager, the Desktop page the wallpaper of every monitor,
//! shared or one by one, and the Theme page the colors of the interface and
//! the look of window frames: a preview of the desktop as it would be, a
//! dark and a light scheme, three styles of frame, the four colors each
//! from a row of presets or from a picker with a spectrum and a hex field
//! that opens over the page and shows the color on screen as it is dragged,
//! and the shading, buttons and corners of windows one by one.  A style
//! also sets the bar's layout and margin and the start menu's layout, which
//! live on the Panel page.  Every change is written to the config file at once, which the desktop then
//! reads back, so the file stays the one source of the settings.  Images
//! are chosen through the file manager in its picker form, and applications
//! for the quick launch and favorites lists from a list that opens in place.
//! A page taller than the window scrolls.

use tiny_skia::{Color, PathBuilder};

use crate::config::{
    self, BackgroundStyle, ButtonSide, COLOR_KEYS, Config, FILES_KEYS, Glyphs, MenuLayout, PANEL_KEYS,
    Surface, WALLPAPER_KEYS, WINDOW_KEYS, WallpaperMode,
};
use crate::desktop::{Desktop, Hover, Popup};
use crate::event::Button;
use crate::geom::Rect;
use crate::i18n::tr;
use crate::render::{Align, Canvas, color_to_hsv, hsv_to_color, parse_color};
use crate::ui::{self, popups, preview};
use crate::xkb::Key;

/// Settings window width, with a gutter for the page's scrollbar.
const WIDTH: i32 = 700 + ui::SCROLLBAR;
/// Settings window height, grown when a page needs more and the screen has it.
const HEIGHT: i32 = 460;
/// Inner padding of the window.
const PAD: i32 = 16;
/// Height of one line of a setting row.
const ROW: i32 = 44;
/// Height of a control.
const CONTROL: i32 = 28;
/// Horizontal inset of a card's rows.
const CARD_PAD: i32 = 12;
/// Corner radius of a card.
const CARD_RADIUS: f32 = 10.0;
/// Height of a group's caption line.
const CAPTION: i32 = 28;
/// Space between a card and the next caption.
const GROUP_GAP: i32 = 12;
/// Width of the name column of a row.
const TITLE_WIDTH: i32 = 150;
/// Width of the page column.
const NAV_WIDTH: i32 = 136;
/// Height of a page row.
const NAV_ROW: i32 = 32;
/// Side of an icon chip.
const CHIP: i32 = 30;
/// Height of an application list row.
const LIST_ROW: i32 = 30;
/// Width of the right-click menu.
const MENU_WIDTH: i32 = 220;
/// Row height of the right-click menu.
const MENU_ROW: i32 = 30;
/// Space between controls flowed in a row.
const GAP: i32 = 4;
/// Width of a toggle switch.
const SWITCH_WIDTH: i32 = 42;
/// Height of a toggle switch.
const SWITCH_HEIGHT: i32 = 22;
/// Width of a slider with its value.
const SLIDER_WIDTH: i32 = 230;
/// Width of the value shown beside a slider.
const SLIDER_VALUE_WIDTH: i32 = 46;
/// Diameter of a slider's knob.
const KNOB: i32 = 16;
/// Width of a text field.
const FIELD_WIDTH: i32 = 200;
/// The least width a text field shrinks to for the buttons beside it.
const FIELD_MIN_WIDTH: i32 = 120;
/// Width of the swatch of a color's current value, which opens its picker.
const PICKER_BUTTON: i32 = 30;
/// Width of the color picker.
const PICKER_WIDTH: i32 = 216;
/// Inner padding of the color picker.
const PICKER_PAD: i32 = 8;
/// Height of the picker's saturation and value square.
const PICKER_AREA: i32 = 140;
/// Height of the picker's hue strip.
const PICKER_HUE: i32 = 16;
/// Width of the picker's hex field.
const PICKER_FIELD_WIDTH: i32 = 96;
/// Radius of the ring marking the chosen point on the picker's square.
const PICKER_MARK: f32 = 6.0;
/// Height of the color picker.
const PICKER_HEIGHT: i32 = PICKER_AREA + PICKER_HUE + CONTROL + 4 * PICKER_PAD;
/// Side of a color swatch.
const SWATCH: i32 = 22;

/// The sliders of the Panel and Theme pages: key, lowest, highest, step
/// and unit.
const SLIDERS: [(&str, i32, i32, i32, &str); 5] = [
    (
        "panel_height",
        config::MIN_PANEL_HEIGHT,
        config::MAX_PANEL_HEIGHT,
        2,
        "px",
    ),
    ("panel_width", 20, 100, 5, "%"),
    ("panel_opacity", 0, 100, 5, "%"),
    ("panel_margin", 0, config::MAX_PANEL_MARGIN, 2, "px"),
    ("window_corners", 0, config::MAX_CORNERS, 1, "px"),
];

/// The color schemes: the four colors of each, as accent, bar, menus and
/// text.
const SCHEMES: [(&str, [&str; 4]); 2] = [
    ("Dark", ["#7967c5", "#0a0e17", "#121826", "#eef2f8"]),
    ("Light", ["#6b5bd6", "#e4e7ee", "#f5f6f9", "#1b2030"]),
];

/// A style: a name and the frame, bar and menu settings it bundles.
struct Style {
    name: &'static str,
    surface: Surface,
    buttons: ButtonSide,
    glyphs: Glyphs,
    corners: i32,
    centered: bool,
    margin: i32,
    menu: MenuLayout,
}

impl Style {
    /// Whether every setting of the style is in effect.
    fn matches(&self, config: &Config) -> bool {
        let window = config.window;
        window.surface == self.surface
            && window.buttons == self.buttons
            && window.glyphs == self.glyphs
            && window.corners == self.corners
            && config.panel_centered == self.centered
            && config.panel_margin == self.margin
            && config.menu_layout == self.menu
    }

    /// The keys and values the style writes.
    fn changes(&self) -> Vec<(&'static str, String)> {
        vec![
            ("window_surface", surface_value(self.surface).to_string()),
            (
                "window_buttons",
                match self.buttons {
                    ButtonSide::Right => "right",
                    ButtonSide::Left => "left",
                }
                .to_string(),
            ),
            ("window_glyphs", glyphs_value(self.glyphs).to_string()),
            ("window_corners", self.corners.to_string()),
            (
                "panel_layout",
                if self.centered { "centered" } else { "left" }.to_string(),
            ),
            ("panel_margin", self.margin.to_string()),
            ("menu_layout", menu_value(self.menu).to_string()),
        ]
    }
}

/// The styles: the Linux desktop as it is, the Fruit look with a floating
/// dock, circle buttons and a grid of applications, and the Redmond look
/// with square frames, wide buttons and a tiled start menu.
const STYLES: [Style; 3] = [
    Style {
        name: "Linux",
        surface: Surface::Flat,
        buttons: ButtonSide::Right,
        glyphs: Glyphs::Lines,
        corners: 8,
        centered: false,
        margin: 0,
        menu: MenuLayout::Columns,
    },
    Style {
        name: "Fruit",
        surface: Surface::Glossy,
        buttons: ButtonSide::Left,
        glyphs: Glyphs::Circles,
        corners: 10,
        centered: false,
        margin: 8,
        menu: MenuLayout::Grid,
    },
    Style {
        name: "Redmond",
        surface: Surface::Flat,
        buttons: ButtonSide::Right,
        glyphs: Glyphs::Wide,
        corners: 0,
        centered: false,
        margin: 0,
        menu: MenuLayout::Tiles,
    },
];

/// Space above and below the preview inside its card.
const PREVIEW_PAD: i32 = 10;

/// Preset background colors, true black first.
const SWATCHES: [&str; 12] = [
    "#000000", "#101418", "#1d222b", "#2b2b2b", "#1e2a3a", "#243b2f", "#3b2d4a", "#5a5a5a", "#0b1f3a",
    "#2d1b33", "#4a2b1a", "#1a1a2e",
];
/// Preset accent colors, the default first.
const ACCENTS: [&str; 12] = [
    "#7967c5", "#9d8cf0", "#3d7dd6", "#2f9e6e", "#d5499a", "#e0803a", "#d64545", "#8a8f99", "#1fa8b8",
    "#c9a227", "#6c8ebf", "#b56576",
];
/// Preset surface colors for the bar and the menus, the defaults first and
/// the light ones last.
const SURFACES: [&str; 12] = [
    "#0a0e17", "#121826", "#000000", "#1d222b", "#2b2b2b", "#1e2a3a", "#3b2d4a", "#2e3440", "#e4e7ee",
    "#f5f6f9", "#f2f3f5", "#fdf6e3",
];
/// Preset text colors, the default first and the dark ones last.
const TEXTS: [&str; 12] = [
    "#eef2f8", "#ffffff", "#d0d4da", "#b6c0d2", "#f5e6c8", "#c8e6ff", "#ffd166", "#93a1a1", "#586e75",
    "#333333", "#1b2030", "#111111",
];

/// The pages of the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    /// The bar and the start menu.
    Panel,
    /// The file manager.
    Files,
    /// The wallpaper.
    Desktop,
    /// The colors and the window frames.
    Theme,
}

impl Page {
    /// Every page in order.
    const ALL: [Page; 4] = [Page::Panel, Page::Files, Page::Desktop, Page::Theme];

    /// The page's name.
    fn name(self) -> &'static str {
        match self {
            Page::Panel => tr("Panel"),
            Page::Files => tr("Files"),
            Page::Desktop => tr("Desktop"),
            Page::Theme => tr("Theme"),
        }
    }
}

/// The colors of the interface the user can set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeColor {
    /// Markers, selections and switches.
    Accent,
    /// The bar.
    Panel,
    /// Menus, popups and windows.
    Popup,
    /// Text.
    Text,
}

impl ThemeColor {
    /// Every color in the order shown.
    const ALL: [ThemeColor; 4] = [
        ThemeColor::Accent,
        ThemeColor::Panel,
        ThemeColor::Popup,
        ThemeColor::Text,
    ];

    /// The config key.
    fn key(self) -> &'static str {
        match self {
            ThemeColor::Accent => "color_accent",
            ThemeColor::Panel => "color_panel",
            ThemeColor::Popup => "color_popup",
            ThemeColor::Text => "color_text",
        }
    }

    /// The row's name.
    fn name(self) -> &'static str {
        match self {
            ThemeColor::Accent => tr("Accent"),
            ThemeColor::Panel => tr("Bar"),
            ThemeColor::Popup => tr("Menus"),
            ThemeColor::Text => tr("Text"),
        }
    }

    /// The preset swatches offered.
    fn presets(self) -> &'static [&'static str] {
        match self {
            ThemeColor::Accent => &ACCENTS,
            ThemeColor::Panel | ThemeColor::Popup => &SURFACES,
            ThemeColor::Text => &TEXTS,
        }
    }

    /// The color in use.
    fn current(self, theme: ui::Theme) -> Color {
        match self {
            ThemeColor::Accent => theme.accent,
            ThemeColor::Panel => theme.panel,
            ThemeColor::Popup => theme.popup,
            ThemeColor::Text => theme.text,
        }
    }
}

/// A text field of the settings window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    /// The clock's strftime pattern.
    ClockFormat,
    /// The start button's icon name or image path.
    StartIcon,
    /// The start button's label.
    StartLabel,
    /// The wallpaper image path.
    Image,
    /// The background color.
    Color,
    /// One of the interface colors.
    Theme(ThemeColor),
}

/// What clicking a control does.
#[derive(Clone, Debug, PartialEq)]
enum Action {
    /// Write keys and values; an empty value removes the key.
    Set(Vec<(&'static str, String)>),
    /// Focus a text field.
    Focus(Field),
    /// Show the wallpaper settings of a monitor, or the shared ones.
    Monitor(Option<u32>),
    /// Open the file manager to choose a wallpaper image.
    Browse,
    /// Open the file manager to choose a start button icon.
    BrowseIcon,
    /// Open the settings window, from the bar's menu.
    OpenSettings,
    /// Read the applications again, from the bar's menu.
    Reload,
    /// Close the window.
    Close,
    /// Show a page.
    Page(Page),
    /// Open the application list for a list setting.
    Pick(&'static str),
    /// Return from the application list.
    Back,
    /// Add an application to a list setting, or remove it.
    Toggle(&'static str, String),
    /// Remove the page's settings from the file.
    Reset,
    /// Drag a slider for a key.
    Slide(&'static str),
    /// Open the color picker of a field, or close it when it is open.
    OpenPicker(Field),
    /// Press on the picker's hue strip.
    Hue,
    /// Press on the picker's saturation and value square.
    Area,
}

/// How a control is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// One of a row of choices.
    Segment,
    /// A whole row with a toggle switch at its right.
    Switch,
    /// A text field.
    Text,
    /// A flat button.
    Button,
    /// A color square.
    Swatch,
    /// The cross that closes the window.
    Close,
    /// A page in the left column.
    Nav,
    /// An application icon; a click removes it.
    Chip,
    /// A row of the application list.
    AppRow,
    /// A slider; the label is the value shown.
    Slider,
    /// The swatch of a color's current value, with an eyedropper on it, that
    /// opens its picker.
    Picker,
    /// The picker's hue strip.
    HueStrip,
    /// The picker's saturation and value square.
    ColorArea,
}

/// One clickable thing in the window.
#[derive(Clone, Debug)]
struct Control {
    rect: Rect,
    action: Action,
    label: String,
    active: bool,
    kind: Kind,
    color: Option<Color>,
    /// The application whose icon the control shows.
    app: Option<usize>,
    /// The area the control is confined to when it scrolls.
    clip: Option<Rect>,
}

/// A piece of text in the page: a group caption or a row's name.
struct Text {
    rect: Rect,
    text: String,
    caption: bool,
}

/// What the page area shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum View {
    /// A page of settings.
    Page(Page),
    /// The application list for a list setting.
    List(&'static str),
}

/// Everything laid out in the window.
#[derive(Default)]
struct Layout {
    controls: Vec<Control>,
    texts: Vec<Text>,
    /// Card backgrounds.
    cards: Vec<Rect>,
    /// Hairlines between rows.
    lines: Vec<Rect>,
    /// Where the theme preview is drawn, when the page has it.
    previews: Vec<Rect>,
    /// Height of the page's content before scrolling.
    content: i32,
    /// The open color picker, laid over the page.
    picker: Option<PickerLayout>,
}

/// Where the open color picker is.
#[derive(Clone, Copy, Debug)]
struct PickerLayout {
    /// The picker's frame.
    frame: Rect,
    /// The saturation and value square.
    area: Rect,
    /// The hue strip.
    hue: Rect,
    /// Index of the picker's first control; the ones before belong to the page.
    first: usize,
}

/// The window's state between events.
pub struct SettingsState {
    /// The page shown.
    pub page: Page,
    /// The list setting whose application list is open, if any.
    pub picking: Option<&'static str>,
    /// The monitor whose wallpaper is shown, or `None` for the shared one.
    pub monitor: Option<u32>,
    /// The text field being typed into.
    pub focus: Option<Field>,
    /// Text of the focused field as typed so far.
    pub editing: String,
    /// Scroll offset of the page or the application list.
    pub scroll: f64,
    /// The slider being dragged, with its value so far.
    pub drag: Option<(&'static str, i32)>,
    /// The color picker open for a field, if any.
    pub picker: Option<Picker>,
    /// The scrollbar thumb being dragged, as the offset into it grabbed.
    pub bar_drag: Option<f64>,
}

/// The color picker of a field.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Picker {
    /// The color field picked for.
    pub field: Field,
    /// The color as hue in degrees, saturation and value, kept apart from
    /// the setting so a gray or a black keeps its hue while dragging.
    pub hsv: (f32, f32, f32),
    /// The part of the picker being dragged, if any.
    pub drag: Option<PickerPart>,
}

/// A draggable part of the color picker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickerPart {
    /// The hue strip.
    Hue,
    /// The saturation and value square.
    Area,
}

impl Default for SettingsState {
    fn default() -> SettingsState {
        SettingsState {
            page: Page::Panel,
            picking: None,
            monitor: None,
            focus: None,
            editing: String::new(),
            scroll: 0.0,
            drag: None,
            picker: None,
            bar_drag: None,
        }
    }
}

impl SettingsState {
    /// Resets the window for opening.
    pub fn open(&mut self) {
        self.picking = None;
        self.monitor = None;
        self.focus = None;
        self.editing.clear();
        self.scroll = 0.0;
        self.drag = None;
        self.picker = None;
        self.bar_drag = None;
    }

    /// Clears the state when the window closes.
    pub fn close(&mut self) {
        self.focus = None;
        self.picking = None;
        self.editing.clear();
        self.drag = None;
        self.picker = None;
        self.bar_drag = None;
    }

    /// Whether a slider, a part of the color picker or the scrollbar is
    /// being dragged.
    pub fn dragging(&self) -> bool {
        self.drag.is_some()
            || self.bar_drag.is_some()
            || self.picker.is_some_and(|picker| picker.drag.is_some())
    }
}

/// The items of the bar's right-click menu.
fn panel_menu_items(d: &Desktop) -> Vec<(&'static str, Action)> {
    let position = if d.panel_top() { "bottom" } else { "top" };
    vec![
        (tr("Settings…"), Action::OpenSettings),
        (
            if d.panel_top() {
                tr("Move panel to bottom")
            } else {
                tr("Move panel to top")
            },
            Action::Set(vec![("panel_position", position.to_string())]),
        ),
        (tr("Reload applications"), Action::Reload),
    ]
}

/// The rectangle of the right-click menu anchored at a bar X.
fn panel_menu_rect(d: &Desktop, x: i32) -> Rect {
    let items = panel_menu_items(d).len() as i32;
    let anchor = Rect::new(x - 8, d.bar_rect().y, 16, d.bar_rect().h);
    popups::anchor_above(d, anchor, MENU_WIDTH, items * MENU_ROW + 12)
}

/// Draws the right-click menu; returns its rectangle.
///
/// # Arguments
/// * `d` - The desktop.
/// * `canvas` - The panel canvas.
/// * `x` - The bar X the menu was opened at.
pub fn draw_panel_menu(d: &mut Desktop, canvas: &mut Canvas<'_>, x: i32) -> Rect {
    let frame = panel_menu_rect(d, x);
    ui::draw_popup_frame(canvas, frame);
    for (row, (label, _)) in panel_menu_items(d).into_iter().enumerate() {
        let rect = Rect::new(
            frame.x + 6,
            frame.y + 6 + row as i32 * MENU_ROW,
            frame.w - 12,
            MENU_ROW,
        );
        if d.hover == Hover::PanelMenuItem(row) {
            canvas.fill_rounded(rect, 5.0, ui::hover_bg());
        }
        let label_rect = Rect::new(rect.x + 10, rect.y, rect.w - 20, rect.h);
        ui::label(
            canvas,
            &mut d.fonts,
            label,
            label_rect,
            ui::FONT_SIZE,
            false,
            Align::Left,
            ui::text(),
        );
    }
    frame
}

/// The right-click menu item under a point, or `None` outside the menu.
///
/// # Arguments
/// * `d` - The desktop.
/// * `x` - Pointer X.
/// * `y` - Pointer Y.
pub fn panel_menu_hit(d: &Desktop, x: f64, y: f64) -> Option<Hover> {
    let Popup::PanelMenu(at) = d.popup else {
        return None;
    };
    let frame = panel_menu_rect(d, at);
    if !frame.contains(x, y) {
        return None;
    }
    let row = ((y - frame.y as f64 - 6.0) / MENU_ROW as f64).floor();
    if row >= 0.0 && (row as usize) < panel_menu_items(d).len() {
        return Some(Hover::PanelMenuItem(row as usize));
    }
    Some(Hover::None)
}

/// Handles a click; returns whether it landed in the right-click menu.
///
/// # Arguments
/// * `d` - The desktop.
/// * `x` - Pointer X.
/// * `y` - Pointer Y.
/// * `button` - The button.
pub fn panel_menu_click(d: &mut Desktop, x: f64, y: f64, button: Button) -> bool {
    let Some(hover) = panel_menu_hit(d, x, y) else {
        return false;
    };
    if button != Button::Left {
        return true;
    }
    if let Hover::PanelMenuItem(row) = hover
        && let Some((_, action)) = panel_menu_items(d).get(row).cloned()
    {
        match action {
            Action::OpenSettings => {
                d.close_popup();
                d.open_popup(Popup::Settings);
            }
            Action::Set(changes) => {
                d.close_popup();
                d.write_config(&changes);
            }
            _ => {
                d.close_popup();
                d.reload();
            }
        }
    }
    true
}

/// The config key of a wallpaper setting for one monitor.  The handful of
/// distinct names are leaked once each so they can be static like the rest.
fn monitor_key(base: &'static str, monitor: u32) -> &'static str {
    use std::sync::{Mutex, OnceLock};
    type Known = Vec<(&'static str, u32, &'static str)>;
    static KEYS: OnceLock<Mutex<Known>> = OnceLock::new();
    let keys = KEYS.get_or_init(|| Mutex::new(Vec::new()));
    let mut keys = keys.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((_, _, key)) = keys.iter().find(|(b, m, _)| *b == base && *m == monitor) {
        return key;
    }
    let key: &'static str = Box::leak(format!("{base}_{monitor}").into_boxed_str());
    keys.push((base, monitor, key));
    key
}

/// The config key a wallpaper setting is written to for the monitor shown.
fn wallpaper_key(d: &Desktop, base: &'static str) -> &'static str {
    match d.settings.monitor {
        None => base,
        Some(n) => monitor_key(base, n),
    }
}

/// The settings window rectangle for a height.
fn frame_for(d: &Desktop, height: i32) -> Rect {
    let bar = d.bar_rect();
    let room = d
        .primary_background()
        .filter(|b| b.geometry.configured)
        .map_or(d.panel.height - bar.h, |b| {
            b.icon_area().h.min(d.panel.height - bar.h)
        });
    let height = height.min(room - ui::POPUP_MARGIN * 2).max(240);
    let width = WIDTH.min(d.panel.width - 2 * ui::POPUP_MARGIN);
    let x = ((d.panel.width - width) / 2).max(ui::POPUP_MARGIN);
    let y = if d.panel_top() {
        bar.bottom() + ui::POPUP_MARGIN
    } else {
        bar.y - ui::POPUP_MARGIN - height
    };
    Rect::new(x, y, width, height)
}

/// The settings window rectangle: tall enough for the tallest page when
/// the screen allows, so it keeps its size from page to page, scrolled
/// otherwise.
fn frame(d: &Desktop) -> Rect {
    let probe = frame_for(d, HEIGHT);
    let content = Page::ALL
        .iter()
        .map(|page| build(d, probe, 0.0, View::Page(*page)).content)
        .max()
        .unwrap_or(0);
    let needed = content + (probe.h - page_rect_in(probe).h);
    frame_for(d, needed.max(HEIGHT))
}

/// The page area to the right of the page column, for a frame: below the
/// title, above the footer with the Reset or Back button, and short of the
/// gutter its scrollbar sits in.
fn page_rect_in(frame: Rect) -> Rect {
    let x = frame.x + PAD + NAV_WIDTH + PAD;
    let y = frame.y + PAD + CONTROL + 10;
    let bottom = frame.bottom() - PAD - CONTROL - 10;
    Rect::new(x, y, frame.right() - PAD - ui::SCROLLBAR - x, bottom - y)
}

/// The wallpaper the page shows: the monitor's, or the shared one.
fn shown_wallpaper(d: &Desktop) -> config::Wallpaper {
    match d.settings.monitor {
        None => d.config.wallpaper.clone(),
        Some(n) => d.config.wallpaper_for(n),
    }
}

/// A color as `#rrggbb`.
fn hex(color: Color) -> String {
    let c = color.to_color_u8();
    format!("#{:02x}{:02x}{:02x}", c.red(), c.green(), c.blue())
}

/// The text a field shows: what is being typed when it has focus, the
/// setting otherwise.
fn field_text(d: &Desktop, field: Field) -> String {
    if d.settings.focus == Some(field) {
        return d.settings.editing.clone();
    }
    match field {
        Field::ClockFormat => d.config.clock_format.clone(),
        Field::StartIcon => d.config.start_icon.clone().unwrap_or_default(),
        Field::StartLabel => d.config.start_label.clone().unwrap_or_default(),
        Field::Image => shown_wallpaper(d)
            .image
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default(),
        Field::Color | Field::Theme(_) => hex(field_color(d, field)),
    }
}

/// The color a color field is set to; black for a field that is not one.
fn field_color(d: &Desktop, field: Field) -> Color {
    match field {
        Field::Color => shown_wallpaper(d).color,
        Field::Theme(color) => color.current(d.config.theme),
        Field::ClockFormat | Field::StartIcon | Field::StartLabel | Field::Image => Color::BLACK,
    }
}

/// Width of a segment or button for its label; wide scripts count double.
fn chip_width(label: &str) -> i32 {
    let width: i32 = label
        .chars()
        .map(|c| {
            if c.is_ascii() {
                8
            } else if c as u32 >= 0x2E80 {
                14
            } else {
                9
            }
        })
        .sum();
    14 + width + 14
}

/// The keys a page's reset removes.
fn page_keys(d: &Desktop) -> Vec<&'static str> {
    match d.settings.page {
        Page::Panel => PANEL_KEYS.to_vec(),
        Page::Files => FILES_KEYS.to_vec(),
        Page::Desktop => WALLPAPER_KEYS.iter().map(|k| wallpaper_key(d, k)).collect(),
        Page::Theme => COLOR_KEYS.iter().chain(WINDOW_KEYS.iter()).copied().collect(),
    }
}

/// The scheme whose four colors are set, by name.
fn current_scheme(theme: ui::Theme) -> Option<&'static str> {
    let current = [theme.accent, theme.panel, theme.popup, theme.text].map(hex);
    SCHEMES
        .iter()
        .find(|(_, colors)| {
            colors
                .iter()
                .zip(&current)
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
        })
        .map(|(name, _)| *name)
}

/// The style whose settings are all in effect, by name.
fn current_style(config: &Config) -> Option<&'static str> {
    STYLES
        .iter()
        .find(|style| style.matches(config))
        .map(|style| style.name)
}

/// The layout word a menu layout writes.
fn menu_value(layout: MenuLayout) -> &'static str {
    match layout {
        MenuLayout::Columns => "columns",
        MenuLayout::Grid => "grid",
        MenuLayout::Tiles => "tiles",
    }
}

/// The name of a scheme or style, translated.
fn preset_name(name: &str) -> &'static str {
    match name {
        "Dark" => tr("Dark"),
        "Light" => tr("Light"),
        "Linux" => tr("Linux"),
        "Fruit" => tr("Fruit"),
        _ => tr("Redmond"),
    }
}

/// The surface word a style writes.
fn surface_value(surface: Surface) -> &'static str {
    match surface {
        Surface::Flat => "flat",
        Surface::Gradient => "gradient",
        Surface::Glossy => "glossy",
    }
}

/// The glyphs word a style writes.
fn glyphs_value(glyphs: Glyphs) -> &'static str {
    match glyphs {
        Glyphs::Lines => "lines",
        Glyphs::Circles => "circles",
        Glyphs::Wide => "wide",
    }
}

/// Places items of given widths in an area: on one line flush right when
/// they fit, else left to right over as many lines as needed.
///
/// # Arguments
/// * `x` - Left edge of the area.
/// * `width` - Width of the area.
/// * `widths` - The items' widths.
///
/// # Returns
/// Each item's X and line, and the number of lines.
fn flow(x: i32, width: i32, widths: &[i32]) -> (Vec<(i32, i32)>, i32) {
    let total: i32 = widths.iter().sum::<i32>() + GAP * (widths.len() as i32 - 1).max(0);
    if total <= width {
        let mut cursor = x + width - total;
        let placed = widths
            .iter()
            .map(|w| {
                let at = cursor;
                cursor += w + GAP;
                (at, 0)
            })
            .collect();
        return (placed, 1);
    }
    let mut placed = Vec::new();
    let mut cursor = x;
    let mut line = 0;
    for w in widths {
        if cursor + w > x + width && cursor > x {
            cursor = x;
            line += 1;
        }
        placed.push((cursor, line));
        cursor += w + GAP;
    }
    (placed, line + 1)
}

/// The rectangle of an item placed by `flow`, centered in its line.
fn placed(area: Rect, at: (i32, i32), w: i32, h: i32) -> Rect {
    Rect::new(at.0, area.y + at.1 * ROW + (ROW - h) / 2, w, h)
}

/// Builds a page: captioned cards of rows, each row a name and a control.
struct Groups<'a> {
    out: &'a mut Layout,
    clip: Rect,
    x: i32,
    width: i32,
    /// Where the next caption or row goes, scrolled.
    y: i32,
    /// Top of the open card, if one is open.
    card_top: Option<i32>,
    /// Rows in the open card.
    rows: usize,
    /// Whether anything has been added yet.
    started: bool,
}

impl Groups<'_> {
    /// Width of the control area of a row.
    fn area_width(&self) -> i32 {
        self.width - 2 * CARD_PAD - TITLE_WIDTH
    }

    /// Starts a captioned card.
    fn group(&mut self, caption: &str) {
        self.finish();
        if self.started {
            self.y += GROUP_GAP;
        }
        self.started = true;
        self.out.texts.push(Text {
            rect: Rect::new(self.x + CARD_PAD, self.y, self.width - 2 * CARD_PAD, CAPTION),
            text: caption.to_string(),
            caption: true,
        });
        self.y += CAPTION;
        self.card_top = Some(self.y);
        self.rows = 0;
    }

    /// Closes the open card.
    fn finish(&mut self) {
        if let Some(top) = self.card_top.take() {
            self.out
                .cards
                .push(Rect::new(self.x, top, self.width, self.y - top));
        }
    }

    /// Starts a row with a name; returns the area its control goes in.
    ///
    /// # Arguments
    /// * `title` - The row's name.
    /// * `lines` - Lines of `ROW` height the control takes.
    fn row(&mut self, title: &str, lines: i32) -> Rect {
        if self.card_top.is_none() {
            self.card_top = Some(self.y);
            self.rows = 0;
        }
        let inner_x = self.x + CARD_PAD;
        let inner_w = self.width - 2 * CARD_PAD;
        if self.rows > 0 {
            self.out.lines.push(Rect::new(inner_x, self.y, inner_w, 1));
        }
        self.rows += 1;
        let height = lines.max(1) * ROW;
        if !title.is_empty() {
            self.out.texts.push(Text {
                rect: Rect::new(inner_x, self.y, TITLE_WIDTH - 8, ROW),
                text: title.to_string(),
                caption: false,
            });
        }
        let area = Rect::new(inner_x + TITLE_WIDTH, self.y, inner_w - TITLE_WIDTH, height);
        self.y += height;
        area
    }

    /// Adds a control; returns it for its icon or color.
    fn push(&mut self, kind: Kind, rect: Rect, action: Action, label: &str, active: bool) -> &mut Control {
        self.out.controls.push(Control {
            rect,
            action,
            label: label.to_string(),
            active,
            kind,
            color: None,
            app: None,
            clip: Some(self.clip),
        });
        let last = self.out.controls.len() - 1;
        &mut self.out.controls[last]
    }

    /// Adds a row whose whole width toggles a boolean key.
    fn switch(&mut self, title: &str, key: &'static str, on: bool) {
        let area = self.row(title, 1);
        let rect = Rect::new(area.x - TITLE_WIDTH, area.y, area.w + TITLE_WIDTH, ROW);
        self.push(
            Kind::Switch,
            rect,
            Action::Set(vec![(key, (!on).to_string())]),
            title,
            on,
        );
    }

    /// Adds a row of choices.
    fn segments(&mut self, title: &str, options: &[(String, Action)], active: &str) {
        let widths: Vec<i32> = options.iter().map(|(l, _)| chip_width(l)).collect();
        let (_, lines) = flow(0, self.area_width(), &widths);
        let area = self.row(title, lines);
        let (at, _) = flow(area.x, area.w, &widths);
        for (((label, action), w), at) in options.iter().zip(widths).zip(at) {
            let rect = placed(area, at, w, CONTROL);
            self.push(Kind::Segment, rect, action.clone(), label, label == active);
        }
    }

    /// Adds a slider row for a numeric key.
    fn slider(&mut self, title: &str, key: &'static str, value: i32, unit: &str) {
        let area = self.row(title, 1);
        let w = SLIDER_WIDTH.min(area.w);
        let rect = Rect::new(area.right() - w, area.y + (ROW - CONTROL) / 2, w, CONTROL);
        self.push(
            Kind::Slider,
            rect,
            Action::Slide(key),
            &format!("{value}{unit}"),
            false,
        );
    }

    /// Adds a text field with flat buttons after it.  The field gives up
    /// width for the buttons down to a minimum; past that it takes the
    /// whole line and the buttons the next.
    fn field(&mut self, title: &str, field: Field, value: String, focused: bool, buttons: &[(&str, Action)]) {
        let area_w = self.area_width();
        let buttons_w: i32 = buttons.iter().map(|(l, _)| chip_width(l) + GAP).sum();
        let mut widths: Vec<i32> = vec![
            FIELD_WIDTH
                .min(area_w - buttons_w)
                .max(FIELD_MIN_WIDTH.min(area_w)),
        ];
        widths.extend(buttons.iter().map(|(l, _)| chip_width(l)));
        let together = flow(0, area_w, &widths).1 == 1;
        let area = self.row(title, if together { 1 } else { 2 });
        let (at, field_w) = if together {
            (flow(area.x, area.w, &widths).0, widths[0])
        } else {
            let mut at = vec![(area.x, 0)];
            at.extend(
                flow(area.x, area.w, &widths[1..])
                    .0
                    .into_iter()
                    .map(|(x, _)| (x, 1)),
            );
            (at, area.w)
        };
        let rect = placed(area, at[0], field_w, CONTROL);
        self.push(Kind::Text, rect, Action::Focus(field), &value, focused);
        for (((label, action), w), at) in buttons.iter().zip(&widths[1..]).zip(&at[1..]) {
            let rect = placed(area, *at, *w, CONTROL);
            self.push(Kind::Button, rect, action.clone(), label, false);
        }
    }

    /// Adds a row of preset swatches and the swatch of the current color,
    /// which opens the picker, for a color key.
    ///
    /// # Arguments
    /// * `title` - The row's name.
    /// * `field` - The color field.
    /// * `key` - The config key.
    /// * `current` - The color set.
    /// * `open` - Whether the field's picker is open.
    /// * `presets` - The preset colors as `#rrggbb`.
    fn color(
        &mut self,
        title: &str,
        field: Field,
        key: &'static str,
        current: Color,
        open: bool,
        presets: &[&str],
    ) {
        let mut widths: Vec<i32> = presets.iter().map(|_| SWATCH).collect();
        widths.push(PICKER_BUTTON);
        let (_, lines) = flow(0, self.area_width(), &widths);
        let area = self.row(title, lines);
        let (at, _) = flow(area.x, area.w, &widths);
        let current_hex = hex(current);
        for (swatch, at) in presets.iter().zip(&at) {
            let rect = placed(area, *at, SWATCH, SWATCH);
            let active = current_hex.eq_ignore_ascii_case(swatch);
            let color = parse_color(swatch);
            self.push(
                Kind::Swatch,
                rect,
                Action::Set(vec![(key, swatch.to_string())]),
                "",
                active,
            )
            .color = color;
        }
        let rect = placed(area, at[presets.len()], PICKER_BUTTON, SWATCH);
        self.push(Kind::Picker, rect, Action::OpenPicker(field), "", open)
            .color = Some(current);
    }

    /// Adds a row of application chips for a list setting, with a button
    /// that opens the application list.
    fn chips(&mut self, title: &str, key: &'static str, ids: &[String], d: &Desktop) {
        let members: Vec<usize> = ids.iter().filter_map(|id| d.apps.find(id)).collect();
        let mut widths: Vec<i32> = members.iter().map(|_| CHIP).collect();
        widths.push(CONTROL);
        let (_, lines) = flow(0, self.area_width(), &widths);
        let area = self.row(title, lines);
        let (at, _) = flow(area.x, area.w, &widths);
        for (index, at) in members.iter().zip(&at) {
            let entry = &d.apps.entries[*index];
            let rect = placed(area, *at, CHIP, CHIP);
            self.push(
                Kind::Chip,
                rect,
                Action::Toggle(key, entry.id.clone()),
                &entry.name,
                false,
            )
            .app = Some(*index);
        }
        let rect = placed(area, at[members.len()], CONTROL, CONTROL);
        self.push(Kind::Button, rect, Action::Pick(key), "+", false);
    }

    /// Adds a row holding the theme preview, as tall as it needs.
    fn preview(&mut self) {
        if self.card_top.is_none() {
            self.card_top = Some(self.y);
            self.rows = 0;
        }
        self.rows += 1;
        let height = preview::HEIGHT + 2 * PREVIEW_PAD;
        self.out.previews.push(Rect::new(
            self.x + CARD_PAD,
            self.y,
            self.width - 2 * CARD_PAD,
            height,
        ));
        self.y += height;
    }

    /// Adds a row with one flat button at its right.
    fn button(&mut self, title: &str, label: &str, action: Action) {
        let area = self.row(title, 1);
        let w = chip_width(label).min(area.w);
        let rect = Rect::new(area.right() - w, area.y + (ROW - CONTROL) / 2, w, CONTROL);
        self.push(Kind::Button, rect, action, label, false);
    }
}

/// What the page area shows at the moment.
fn view(d: &Desktop) -> View {
    d.settings.picking.map_or(View::Page(d.settings.page), View::List)
}

/// Lays out the window as it is.
fn layout(d: &Desktop) -> Layout {
    build(d, frame(d), d.settings.scroll, view(d))
}

/// The scrollbar of the page area, when the page or the application list
/// is taller than it.
fn scrollbar(d: &Desktop) -> Option<ui::Scrollbar> {
    let frame = frame(d);
    let content = build(d, frame, 0.0, view(d)).content;
    let page = page_rect_in(frame);
    let gutter = Rect::new(page.x, page.y, page.w + ui::SCROLLBAR, page.h);
    ui::Scrollbar::new(gutter, content, d.settings.scroll)
}

/// Lays the window out inside a frame.
///
/// # Arguments
/// * `d` - The desktop.
/// * `frame` - The window rectangle.
/// * `scroll` - How far the page is scrolled.
/// * `view` - The page or the application list laid out.
fn build(d: &Desktop, frame: Rect, scroll: f64, view: View) -> Layout {
    let page = page_rect_in(frame);
    let mut out = Layout::default();
    let fixed = |rect: Rect, action: Action, label: &str, active: bool, kind: Kind| Control {
        rect,
        action,
        label: label.to_string(),
        active,
        kind,
        color: None,
        app: None,
        clip: None,
    };
    out.controls.push(fixed(
        Rect::new(frame.right() - PAD - CONTROL, frame.y + PAD, CONTROL, CONTROL),
        Action::Close,
        "",
        false,
        Kind::Close,
    ));
    for (i, item) in Page::ALL.iter().enumerate() {
        out.controls.push(fixed(
            Rect::new(frame.x + PAD, page.y + i as i32 * NAV_ROW, NAV_WIDTH, NAV_ROW),
            Action::Page(*item),
            item.name(),
            d.settings.page == *item,
            Kind::Nav,
        ));
    }
    let footer = |label: &str| {
        let w = chip_width(label);
        Rect::new(
            frame.right() - PAD - w,
            frame.bottom() - PAD - CONTROL,
            w,
            CONTROL,
        )
    };

    if let View::List(key) = view {
        out.controls.push(fixed(
            footer(tr("Back")),
            Action::Back,
            tr("Back"),
            false,
            Kind::Button,
        ));
        let (title, members) = if key == "favorites" {
            (tr("Favorites"), &d.config.favorites)
        } else {
            (tr("Quick launch"), &d.config.launchers)
        };
        out.texts.push(Text {
            rect: Rect::new(page.x + CARD_PAD, page.y, page.w - 2 * CARD_PAD, CAPTION),
            text: title.to_string(),
            caption: true,
        });
        let list = Rect::new(page.x, page.y + CAPTION, page.w, page.h - CAPTION);
        out.cards.push(list);
        for (row, index) in d.apps.all_sorted().into_iter().enumerate() {
            let y = list.y + row as i32 * LIST_ROW - scroll as i32;
            if y + LIST_ROW < list.y || y > list.bottom() {
                continue;
            }
            let entry = &d.apps.entries[index];
            out.controls.push(Control {
                rect: Rect::new(list.x, y, list.w, LIST_ROW),
                action: Action::Toggle(key, entry.id.clone()),
                label: entry.name.clone(),
                active: members.contains(&entry.id),
                kind: Kind::AppRow,
                color: None,
                app: Some(index),
                clip: Some(list),
            });
        }
        out.content = d.apps.entries.len() as i32 * LIST_ROW + CAPTION;
        return out;
    }

    out.controls.push(fixed(
        footer(tr("Reset")),
        Action::Reset,
        tr("Reset"),
        false,
        Kind::Button,
    ));
    let mut groups = Groups {
        out: &mut out,
        clip: page,
        x: page.x,
        width: page.w,
        y: page.y - scroll as i32,
        card_top: None,
        rows: 0,
        started: false,
    };
    let set = |key: &'static str, value: &str| Action::Set(vec![(key, value.to_string())]);
    let focus = d.settings.focus;
    let picking_color = d.settings.picker.map(|picker| picker.field);
    let View::Page(shown) = view else {
        return out;
    };
    match shown {
        Page::Panel => {
            groups.group(tr("Bar"));
            groups.segments(
                tr("Edge"),
                &[
                    (tr("Bottom").to_string(), set("panel_position", "bottom")),
                    (tr("Top").to_string(), set("panel_position", "top")),
                ],
                if d.panel_top() { tr("Top") } else { tr("Bottom") },
            );
            groups.switch(tr("Auto-hide"), "panel_autohide", d.config.panel_autohide);
            for (label, (key, _, _, _, unit)) in [tr("Height"), tr("Width"), tr("Opacity")]
                .iter()
                .zip(SLIDERS.iter())
            {
                groups.slider(label, key, slider_value(d, key), unit);
            }
            groups.field(
                tr("Icon"),
                Field::StartIcon,
                field_text(d, Field::StartIcon),
                focus == Some(Field::StartIcon),
                &[
                    (tr("Browse…"), Action::BrowseIcon),
                    (tr("Default"), set("start_icon", "")),
                ],
            );
            groups.field(
                tr("Label"),
                Field::StartLabel,
                field_text(d, Field::StartLabel),
                focus == Some(Field::StartLabel),
                &[],
            );
            groups.segments(
                tr("Layout"),
                &[
                    (tr("Left").to_string(), set("panel_layout", "left")),
                    (tr("Centered").to_string(), set("panel_layout", "centered")),
                ],
                if d.config.panel_centered {
                    tr("Centered")
                } else {
                    tr("Left")
                },
            );
            groups.slider(
                tr("Margin"),
                "panel_margin",
                slider_value(d, "panel_margin"),
                "px",
            );
            groups.group(tr("Items"));
            groups.switch(tr("Clock"), "show_clock", d.config.show_clock);
            groups.field(
                tr("Format"),
                Field::ClockFormat,
                field_text(d, Field::ClockFormat),
                focus == Some(Field::ClockFormat),
                &[],
            );
            groups.switch(tr("Volume"), "show_volume", d.config.show_volume);
            groups.switch(tr("Tray"), "show_tray", d.config.show_tray);
            groups.switch(tr("Desktop"), "show_desktop_button", d.config.show_desktop_button);
            groups.group(tr("Quick launch"));
            let launchers = d.config.launchers.clone();
            groups.chips(tr("Applications"), "launchers", &launchers, d);
            groups.group(tr("Start menu"));
            groups.segments(
                tr("Layout"),
                &[
                    (tr("Columns").to_string(), set("menu_layout", "columns")),
                    (tr("Grid").to_string(), set("menu_layout", "grid")),
                    (tr("Tiles").to_string(), set("menu_layout", "tiles")),
                ],
                match d.config.menu_layout {
                    MenuLayout::Columns => tr("Columns"),
                    MenuLayout::Grid => tr("Grid"),
                    MenuLayout::Tiles => tr("Tiles"),
                },
            );
            let favorites = d.config.favorites.clone();
            groups.chips(tr("Favorites"), "favorites", &favorites, d);
            groups.switch(tr("Recent"), "track_recent", d.config.track_recent);
        }
        Page::Files => {
            groups.group(tr("Files"));
            groups.segments(
                tr("View"),
                &[
                    (tr("Icons").to_string(), set("files_view", "icons")),
                    (tr("List").to_string(), set("files_view", "list")),
                ],
                if d.config.files_list {
                    tr("List")
                } else {
                    tr("Icons")
                },
            );
            let sorts: Vec<(String, Action)> = [
                (tr("Name"), "name"),
                (tr("Size"), "size"),
                (tr("Modified"), "modified"),
                (tr("Type"), "type"),
            ]
            .iter()
            .map(|(label, value)| (label.to_string(), set("files_sort", value)))
            .collect();
            let current = match d.config.files_sort.as_str() {
                "size" => tr("Size"),
                "modified" => tr("Modified"),
                "type" => tr("Type"),
                _ => tr("Name"),
            };
            groups.segments(tr("Sort"), &sorts, current);
            groups.switch(tr("Hidden files"), "files_hidden", d.config.files_hidden);
        }
        Page::Desktop => {
            groups.group(tr("Wallpaper"));
            let monitors = d.monitor_list();
            if monitors.len() > 1 {
                let mut options: Vec<(String, Action)> = vec![(tr("All").to_string(), Action::Monitor(None))];
                for n in 1..=monitors.len() as u32 {
                    options.push((n.to_string(), Action::Monitor(Some(n))));
                }
                let active = d
                    .settings
                    .monitor
                    .map_or(tr("All").to_string(), |n| n.to_string());
                groups.segments(tr("Monitor"), &options, &active);
            }
            let wallpaper = shown_wallpaper(d);
            let style_key = wallpaper_key(d, "background_style");
            groups.segments(
                tr("Background"),
                &[
                    (tr("Solid").to_string(), set(style_key, "solid")),
                    (tr("Gradient").to_string(), set(style_key, "gradient")),
                ],
                match wallpaper.style {
                    BackgroundStyle::Solid => tr("Solid"),
                    BackgroundStyle::Gradient => tr("Gradient"),
                },
            );
            groups.color(
                tr("Color"),
                Field::Color,
                wallpaper_key(d, "background"),
                field_color(d, Field::Color),
                picking_color == Some(Field::Color),
                &SWATCHES,
            );
            groups.field(
                tr("Image"),
                Field::Image,
                field_text(d, Field::Image),
                focus == Some(Field::Image),
                &[
                    (tr("Browse…"), Action::Browse),
                    (tr("Clear"), set(wallpaper_key(d, "wallpaper"), "none")),
                ],
            );
            let mode_key = wallpaper_key(d, "wallpaper_mode");
            groups.segments(
                tr("Fit"),
                &[
                    (tr("Fill").to_string(), set(mode_key, "fill")),
                    (tr("Fit").to_string(), set(mode_key, "fit")),
                    (tr("Center").to_string(), set(mode_key, "center")),
                    (tr("Stretch").to_string(), set(mode_key, "stretch")),
                ],
                match wallpaper.mode {
                    WallpaperMode::Fill => tr("Fill"),
                    WallpaperMode::Fit => tr("Fit"),
                    WallpaperMode::Center => tr("Center"),
                    WallpaperMode::Stretch => tr("Stretch"),
                },
            );
            if let Some(n) = d.settings.monitor
                && d.config.monitors.contains_key(&n)
            {
                let clear: Vec<(&'static str, String)> = WALLPAPER_KEYS
                    .iter()
                    .map(|key| (monitor_key(key, n), String::new()))
                    .collect();
                groups.button("", tr("Use shared"), Action::Set(clear));
            }
        }
        Page::Theme => {
            groups.group(tr("Preview"));
            groups.preview();
            groups.group(tr("Theme"));
            let schemes: Vec<(String, Action)> = SCHEMES
                .iter()
                .map(|(name, colors)| {
                    let changes = COLOR_KEYS
                        .iter()
                        .zip(colors)
                        .map(|(key, color)| (*key, color.to_string()))
                        .collect();
                    (preset_name(name).to_string(), Action::Set(changes))
                })
                .collect();
            groups.segments(
                tr("Scheme"),
                &schemes,
                current_scheme(d.config.theme).map_or("", preset_name),
            );
            let styles: Vec<(String, Action)> = STYLES
                .iter()
                .map(|style| (preset_name(style.name).to_string(), Action::Set(style.changes())))
                .collect();
            groups.segments(
                tr("Style"),
                &styles,
                current_style(&d.config).map_or("", preset_name),
            );
            groups.group(tr("Colors"));
            for color in ThemeColor::ALL {
                let field = Field::Theme(color);
                groups.color(
                    color.name(),
                    field,
                    color.key(),
                    field_color(d, field),
                    picking_color == Some(field),
                    color.presets(),
                );
            }
            groups.group(tr("Windows"));
            let window = d.config.window;
            groups.switch(tr("Decorate windows"), "theme_windows", window.decorate);
            groups.segments(
                tr("Surface"),
                &[
                    (tr("Flat").to_string(), set("window_surface", "flat")),
                    (tr("Gradient").to_string(), set("window_surface", "gradient")),
                    (tr("Glossy").to_string(), set("window_surface", "glossy")),
                ],
                match window.surface {
                    Surface::Flat => tr("Flat"),
                    Surface::Gradient => tr("Gradient"),
                    Surface::Glossy => tr("Glossy"),
                },
            );
            groups.segments(
                tr("Buttons"),
                &[
                    (tr("Right").to_string(), set("window_buttons", "right")),
                    (tr("Left").to_string(), set("window_buttons", "left")),
                ],
                match window.buttons {
                    ButtonSide::Right => tr("Right"),
                    ButtonSide::Left => tr("Left"),
                },
            );
            groups.segments(
                tr("Glyphs"),
                &[
                    (tr("Lines").to_string(), set("window_glyphs", "lines")),
                    (tr("Circles").to_string(), set("window_glyphs", "circles")),
                    (tr("Wide").to_string(), set("window_glyphs", "wide")),
                ],
                match window.glyphs {
                    Glyphs::Lines => tr("Lines"),
                    Glyphs::Circles => tr("Circles"),
                    Glyphs::Wide => tr("Wide"),
                },
            );
            groups.slider(
                tr("Corners"),
                "window_corners",
                slider_value(d, "window_corners"),
                "px",
            );
        }
    }
    groups.finish();
    out.content = groups.y + scroll as i32 - page.y;
    if let Some(picker) = d.settings.picker {
        lay_out_picker(d, &mut out, frame, page, picker);
    }
    out
}

/// Lays the color picker over the page: below the swatch that opened it when
/// the window has room there, above it otherwise, and not at all while that
/// swatch is scrolled out of view.
///
/// # Arguments
/// * `d` - The desktop.
/// * `out` - The layout so far, with the page's controls.
/// * `frame` - The window rectangle.
/// * `page` - The page area.
/// * `picker` - The open picker.
fn lay_out_picker(d: &Desktop, out: &mut Layout, frame: Rect, page: Rect, picker: Picker) {
    let Some(anchor) = out
        .controls
        .iter()
        .find(|c| c.action == Action::OpenPicker(picker.field))
        .map(|c| c.rect)
    else {
        return;
    };
    if anchor.bottom() <= page.y || anchor.y >= page.bottom() {
        return;
    }
    let left = frame.x + PAD;
    let x = (anchor.right() - PICKER_WIDTH).clamp(left, (frame.right() - PAD - PICKER_WIDTH).max(left));
    let y = if anchor.bottom() + GAP + PICKER_HEIGHT <= frame.bottom() - PAD {
        anchor.bottom() + GAP
    } else {
        (anchor.y - GAP - PICKER_HEIGHT).max(frame.y + PAD)
    };
    let popup = Rect::new(x, y, PICKER_WIDTH, PICKER_HEIGHT);
    let inner_w = PICKER_WIDTH - 2 * PICKER_PAD;
    let area = Rect::new(popup.x + PICKER_PAD, popup.y + PICKER_PAD, inner_w, PICKER_AREA);
    let hue = Rect::new(area.x, area.bottom() + PICKER_PAD, inner_w, PICKER_HUE);
    let field = Rect::new(area.x, hue.bottom() + PICKER_PAD, PICKER_FIELD_WIDTH, CONTROL);
    let first = out.controls.len();
    let control = |rect, action, label: String, active, kind| Control {
        rect,
        action,
        label,
        active,
        kind,
        color: None,
        app: None,
        clip: None,
    };
    out.controls
        .push(control(area, Action::Area, String::new(), false, Kind::ColorArea));
    out.controls
        .push(control(hue, Action::Hue, String::new(), false, Kind::HueStrip));
    out.controls.push(control(
        field,
        Action::Focus(picker.field),
        field_text(d, picker.field),
        d.settings.focus == Some(picker.field),
        Kind::Text,
    ));
    out.picker = Some(PickerLayout {
        frame: popup,
        area,
        hue,
        first,
    });
}

/// The value a slider shows: the one being dragged, else the setting.
fn slider_value(d: &Desktop, key: &str) -> i32 {
    if let Some((dragging, value)) = d.settings.drag
        && dragging == key
    {
        return value;
    }
    match key {
        "panel_height" => d.config.panel_height,
        "panel_width" => d.config.panel_width,
        "panel_margin" => d.config.panel_margin,
        "window_corners" => d.config.window.corners,
        _ => d.config.panel_opacity,
    }
}

/// The track of a slider inside its control.
fn slider_track(rect: Rect) -> Rect {
    Rect::new(
        rect.x + KNOB / 2,
        rect.y + rect.h / 2 - 2,
        rect.w - SLIDER_VALUE_WIDTH - KNOB,
        4,
    )
}

/// The value of a slider for a pointer X over it, snapped to its step.
fn slider_at(key: &str, rect: Rect, x: f64) -> i32 {
    let Some((_, min, max, step, _)) = SLIDERS.iter().find(|(k, ..)| *k == key) else {
        return 0;
    };
    let track = slider_track(rect);
    let ratio = ((x - track.x as f64) / track.w as f64).clamp(0.0, 1.0);
    let raw = *min as f64 + ratio * (*max - *min) as f64;
    let snapped = ((raw / *step as f64).round() as i32) * step;
    snapped.clamp(*min, *max)
}

/// The control rectangle of the slider for a key, when its page is shown.
fn slider_rect(d: &Desktop, key: &'static str) -> Option<Rect> {
    layout(d)
        .controls
        .iter()
        .find(|c| c.action == Action::Slide(key))
        .map(|c| c.rect)
}

/// A check mark path inside a box.
fn check_path(rect: Rect) -> Option<tiny_skia::Path> {
    let mut pb = PathBuilder::new();
    let (x, y) = (rect.x as f32, rect.y as f32);
    pb.move_to(x + 4.0, y + 8.5);
    pb.line_to(x + 7.0, y + 11.5);
    pb.line_to(x + 12.5, y + 5.0);
    pb.finish()
}

/// A cross path centered in a box.
fn cross_path(rect: Rect, reach: f32) -> Option<tiny_skia::Path> {
    let mut pb = PathBuilder::new();
    let (cx, cy) = (
        rect.x as f32 + rect.w as f32 / 2.0,
        rect.y as f32 + rect.h as f32 / 2.0,
    );
    pb.move_to(cx - reach, cy - reach);
    pb.line_to(cx + reach, cy + reach);
    pb.move_to(cx + reach, cy - reach);
    pb.line_to(cx - reach, cy + reach);
    pb.finish()
}

/// Draws a flat button: a tonal pill with its label.
fn draw_button(d: &mut Desktop, canvas: &mut Canvas<'_>, control: &Control, hovered: bool) {
    let rect = control.rect;
    canvas.fill_rounded(rect, 7.0, ui::text_alpha(if hovered { 0x2a } else { 0x16 }));
    ui::label(
        canvas,
        &mut d.fonts,
        &control.label,
        Rect::new(rect.x + 4, rect.y, rect.w - 8, rect.h),
        ui::FONT_SIZE,
        false,
        Align::Center,
        ui::text(),
    );
}

/// Draws a text field with its caret when focused.
fn draw_field(d: &mut Desktop, canvas: &mut Canvas<'_>, control: &Control, hovered: bool) {
    let rect = control.rect;
    canvas.fill_rounded(rect, 7.0, ui::text_alpha(0x10));
    if control.active {
        canvas.stroke_rounded(rect, 7.0, 1.5, ui::accent());
    } else if hovered {
        canvas.stroke_rounded(rect, 7.0, 1.0, ui::text_dim());
    } else {
        canvas.stroke_rounded(rect, 7.0, 1.0, ui::popup_border());
    }
    let text_rect = Rect::new(rect.x + 8, rect.y, rect.w - 16, rect.h);
    let empty = control.label.is_empty();
    let shown = if empty && !control.active {
        "none".to_string()
    } else {
        control.label.clone()
    };
    let color = if empty { ui::text_dim() } else { ui::text() };
    ui::label(
        canvas,
        &mut d.fonts,
        &shown,
        text_rect,
        ui::FONT_SIZE,
        false,
        Align::Left,
        color,
    );
    if control.active {
        let caret_x = text_rect.x
            + ui::text_width(&mut d.fonts, &control.label, ui::FONT_SIZE, false, d.panel.scale)
                .min(text_rect.w - 2);
        canvas.fill_rect(
            Rect::new(caret_x + 1, text_rect.y + 7, 1, text_rect.h - 14),
            ui::text(),
        );
    }
}

/// The eyedropper glyph centered in a rectangle, as the shapes to fill and
/// the lines to stroke with their widths: a tube tapering to its tip at the
/// lower left, the collar across it, the bulb beyond, and a drop below the
/// tip.
fn dropper_glyph(rect: Rect) -> (Vec<tiny_skia::Path>, Vec<(tiny_skia::Path, f32)>) {
    let scale = 1.05;
    let cx = rect.x as f32 + rect.w as f32 / 2.0 + 0.5 * scale;
    let cy = rect.y as f32 + rect.h as f32 / 2.0 - 1.8 * scale;
    let axis = std::f32::consts::FRAC_1_SQRT_2 * scale;
    let at = |u: f32, v: f32| (cx + (u + v) * axis, cy + (v - u) * axis);
    let polygon = |points: &[(f32, f32)]| {
        let mut pb = PathBuilder::new();
        for (i, (u, v)) in points.iter().enumerate() {
            let (x, y) = at(*u, *v);
            if i == 0 {
                pb.move_to(x, y);
            } else {
                pb.line_to(x, y);
            }
        }
        pb.close();
        pb.finish()
    };
    let line = |u1: f32, v1: f32, u2: f32, v2: f32| {
        let mut pb = PathBuilder::new();
        let (x, y) = at(u1, v1);
        pb.move_to(x, y);
        let (x, y) = at(u2, v2);
        pb.line_to(x, y);
        pb.finish()
    };
    let tube = polygon(&[
        (-7.5, -0.4),
        (-7.5, 0.4),
        (-5.0, 1.5),
        (1.5, 1.5),
        (1.5, -1.5),
        (-5.0, -1.5),
    ]);
    let (tip_x, tip_y) = at(-7.5, 0.0);
    let drop = PathBuilder::from_circle(tip_x - 0.3 * scale, tip_y + 3.2 * scale, 1.3 * scale);
    let fills = [tube, drop].into_iter().flatten().collect();
    let strokes = [
        (line(2.6, -3.4, 2.6, 3.4), 2.4 * scale),
        (line(4.0, 0.0, 6.6, 0.0), 3.6 * scale),
    ]
    .into_iter()
    .filter_map(|(path, width)| path.map(|p| (p, width)))
    .collect();
    (fills, strokes)
}

/// Draws a toggle switch row: the pill at the row's right.
fn draw_switch(canvas: &mut Canvas<'_>, control: &Control, hovered: bool) {
    let rect = control.rect;
    if hovered {
        canvas.fill_rounded(rect, 6.0, ui::hover_bg());
    }
    let pill = Rect::new(
        rect.right() - SWITCH_WIDTH,
        rect.y + (rect.h - SWITCH_HEIGHT) / 2,
        SWITCH_WIDTH,
        SWITCH_HEIGHT,
    );
    let radius = SWITCH_HEIGHT as f32 / 2.0;
    let track = if control.active {
        ui::accent()
    } else {
        ui::text_alpha(0x30)
    };
    canvas.fill_rounded(pill, radius, track);
    let knob = SWITCH_HEIGHT - 6;
    let knob_x = if control.active {
        pill.right() - 3 - knob
    } else {
        pill.x + 3
    };
    canvas.fill_rounded(
        Rect::new(knob_x, pill.y + 3, knob, knob),
        knob as f32 / 2.0,
        if control.active {
            ui::on_accent()
        } else {
            ui::text()
        },
    );
}

/// Draws a slider with its value.
fn draw_slider(d: &mut Desktop, canvas: &mut Canvas<'_>, control: &Control, hovered: bool) {
    let Action::Slide(key) = control.action else {
        return;
    };
    let Some((_, min, max, _, _)) = SLIDERS.iter().find(|(k, ..)| *k == key) else {
        return;
    };
    let rect = control.rect;
    let value = slider_value(d, key);
    let track = slider_track(rect);
    let ratio = (value - min) as f64 / (max - min).max(1) as f64;
    let fill_w = (track.w as f64 * ratio) as i32;
    canvas.fill_rounded(track, 2.0, ui::text_alpha(0x30));
    canvas.fill_rounded(Rect::new(track.x, track.y, fill_w, track.h), 2.0, ui::accent());
    let dragging = d.settings.drag.is_some_and(|(k, _)| k == key);
    let size = if hovered || dragging { KNOB + 2 } else { KNOB };
    let knob = Rect::new(
        track.x + fill_w - size / 2,
        track.y + track.h / 2 - size / 2,
        size,
        size,
    );
    canvas.fill_rounded(knob, size as f32 / 2.0, ui::text());
    ui::label(
        canvas,
        &mut d.fonts,
        &control.label,
        Rect::new(
            rect.right() - SLIDER_VALUE_WIDTH,
            rect.y,
            SLIDER_VALUE_WIDTH,
            rect.h,
        ),
        ui::FONT_SMALL,
        false,
        Align::Right,
        ui::text_dim(),
    );
}

/// Draws one control.
fn draw_control(d: &mut Desktop, canvas: &mut Canvas<'_>, control: &Control, hovered: bool) {
    let rect = control.rect;
    match control.kind {
        Kind::Nav => {
            if control.active {
                canvas.fill_rounded(rect, 6.0, ui::active_bg());
            } else if hovered {
                canvas.fill_rounded(rect, 6.0, ui::hover_bg());
            }
            ui::label(
                canvas,
                &mut d.fonts,
                &control.label,
                Rect::new(rect.x + 10, rect.y, rect.w - 20, rect.h),
                ui::FONT_SIZE,
                control.active,
                Align::Left,
                ui::text(),
            );
        }
        Kind::AppRow => {
            if hovered {
                canvas.fill_rounded(rect, 5.0, ui::hover_bg());
            }
            let icon_rect = Rect::new(
                rect.x + 8,
                rect.y + (rect.h - ui::ICON_SIZE) / 2,
                ui::ICON_SIZE,
                ui::ICON_SIZE,
            );
            if let Some(icon) = control.app.and_then(|i| d.app_icon(i)) {
                canvas.draw_image_fit(&icon, icon_rect);
            }
            ui::label(
                canvas,
                &mut d.fonts,
                &control.label,
                Rect::new(icon_rect.right() + 10, rect.y, rect.w - 60, rect.h),
                ui::FONT_SIZE,
                false,
                Align::Left,
                ui::text(),
            );
            if control.active {
                let box_rect = Rect::new(rect.right() - 28, rect.y + (rect.h - 16) / 2, 16, 16);
                canvas.fill_rounded(box_rect, 4.0, ui::accent());
                if let Some(path) = check_path(box_rect) {
                    canvas.stroke_path(&path, 2.0, ui::on_accent());
                }
            }
        }
        Kind::Chip => {
            canvas.fill_rounded(rect, 6.0, ui::text_alpha(if hovered { 0x2a } else { 0x16 }));
            let icon_rect = rect.inflate(-5);
            if let Some(icon) = control.app.and_then(|i| d.app_icon_sized(i, CHIP - 10)) {
                canvas.draw_image_fit(&icon, icon_rect);
            }
            if hovered {
                canvas.fill_rounded(rect, 6.0, ui::text_alpha(0xa0));
                if let Some(path) = cross_path(rect, 5.0) {
                    canvas.stroke_path(&path, 2.0, ui::popup_bg());
                }
            }
        }
        Kind::Text => draw_field(d, canvas, control, hovered),
        Kind::Swatch => {
            if let Some(color) = control.color {
                canvas.fill_rounded(rect, 5.0, color);
            }
            let border = if control.active {
                ui::accent()
            } else if hovered {
                ui::text()
            } else {
                ui::popup_border()
            };
            canvas.stroke_rounded(rect, 5.0, if control.active { 2.0 } else { 1.0 }, border);
        }
        Kind::Switch => draw_switch(canvas, control, hovered),
        Kind::Close => {
            if hovered {
                canvas.fill_rounded(rect, 5.0, ui::hover_bg());
            }
            if let Some(path) = cross_path(rect, 5.0) {
                canvas.stroke_path(&path, 1.8, ui::text());
            }
        }
        Kind::Slider => draw_slider(d, canvas, control, hovered),
        Kind::Picker => {
            let color = control.color.unwrap_or(Color::BLACK);
            canvas.fill_rounded(rect, 5.0, color);
            let ink = if ui::luminance(color) > 0.5 {
                Color::from_rgba8(0, 0, 0, 0xd0)
            } else {
                Color::from_rgba8(0xff, 0xff, 0xff, 0xe0)
            };
            let (fills, strokes) = dropper_glyph(rect);
            for path in &fills {
                canvas.fill_path(path, ink);
            }
            for (path, width) in &strokes {
                canvas.stroke_path(path, *width, ink);
            }
            let border = if control.active {
                ui::accent()
            } else if hovered {
                ui::text()
            } else {
                ui::popup_border()
            };
            canvas.stroke_rounded(rect, 5.0, if control.active { 2.0 } else { 1.0 }, border);
        }
        Kind::ColorArea => {
            let (h, s, v) = d.settings.picker.map_or((0.0, 0.0, 0.0), |picker| picker.hsv);
            canvas.fill_rounded_shaded(rect, 5.0, |u, w| hsv_to_color(h, u, 1.0 - w));
            canvas.stroke_rounded(rect, 5.0, 1.0, ui::popup_border());
            let cx = rect.x as f32 + s * rect.w as f32;
            let cy = rect.y as f32 + (1.0 - v) * rect.h as f32;
            if let Some(ring) = PathBuilder::from_circle(cx, cy, PICKER_MARK) {
                canvas.stroke_path(&ring, 3.0, Color::from_rgba8(0, 0, 0, 0x80));
                canvas.stroke_path(&ring, 1.5, Color::WHITE);
            }
        }
        Kind::HueStrip => {
            let h = d.settings.picker.map_or(0.0, |picker| picker.hsv.0);
            canvas.fill_rounded_shaded(rect, 5.0, |u, _| hsv_to_color(u * 360.0, 1.0, 1.0));
            canvas.stroke_rounded(rect, 5.0, 1.0, ui::popup_border());
            let x = rect.x + (h / 360.0 * rect.w as f32).round() as i32;
            let knob = Rect::new(x - 3, rect.y - 2, 6, rect.h + 4);
            canvas.fill_rounded(knob, 3.0, Color::WHITE);
            canvas.stroke_rounded(knob, 3.0, 1.0, Color::from_rgba8(0, 0, 0, 0x80));
        }
        Kind::Segment => {
            if control.active {
                canvas.fill_rounded(rect, 7.0, ui::accent());
            } else if hovered {
                canvas.fill_rounded(rect, 7.0, ui::hover_bg());
            }
            ui::label(
                canvas,
                &mut d.fonts,
                &control.label,
                Rect::new(rect.x + 4, rect.y, rect.w - 8, rect.h),
                ui::FONT_SIZE,
                control.active,
                Align::Center,
                if control.active {
                    ui::on_accent()
                } else {
                    ui::text()
                },
            );
        }
        Kind::Button => draw_button(d, canvas, control, hovered),
    }
}

/// Draws the settings window; returns its rectangle.
///
/// # Arguments
/// * `d` - The desktop.
/// * `canvas` - The panel canvas.
pub fn draw(d: &mut Desktop, canvas: &mut Canvas<'_>) -> Rect {
    sync_picker(d);
    let frame = frame(d);
    let page = page_rect_in(frame);
    ui::draw_popup_frame(canvas, frame);
    ui::label(
        canvas,
        &mut d.fonts,
        tr("Settings"),
        Rect::new(frame.x + PAD, frame.y + PAD, frame.w - 2 * PAD - CONTROL, CONTROL),
        ui::FONT_SIZE + 3.0,
        true,
        Align::Left,
        ui::text(),
    );
    canvas.fill_rect(Rect::new(page.x - PAD / 2, page.y, 1, page.h), ui::separator());
    let layout = layout(d);
    let first = layout.picker.map_or(layout.controls.len(), |picker| picker.first);
    for (index, control) in layout.controls.iter().enumerate().take(first) {
        if control.clip.is_none() {
            draw_control(d, canvas, control, d.hover == Hover::Control(index));
        }
    }
    canvas.clip(page);
    for card in &layout.cards {
        canvas.fill_rounded(*card, CARD_RADIUS, ui::field_bg());
    }
    for line in &layout.lines {
        canvas.fill_rect(*line, ui::separator());
    }
    for area in &layout.previews {
        if area.bottom() > page.y && area.y < page.bottom() {
            preview::draw(d, canvas, *area, page);
        }
    }
    for text in &layout.texts {
        ui::label(
            canvas,
            &mut d.fonts,
            &text.text,
            text.rect,
            ui::FONT_SIZE,
            text.caption,
            Align::Left,
            if text.caption { ui::text_dim() } else { ui::text() },
        );
    }
    for (index, control) in layout.controls.iter().enumerate().take(first) {
        if let Some(clip) = control.clip {
            if clip != page {
                canvas.clip(clip);
            }
            draw_control(d, canvas, control, d.hover == Hover::Control(index));
            if clip != page {
                canvas.clip(page);
            }
        }
    }
    canvas.reset_clip();
    if let Some(bar) = scrollbar(d) {
        bar.draw(canvas, d.settings.bar_drag.is_some());
    }
    if let Some(picker) = layout.picker {
        ui::draw_popup_frame(canvas, picker.frame);
        for (index, control) in layout.controls.iter().enumerate().skip(first) {
            draw_control(d, canvas, control, d.hover == Hover::Control(index));
        }
    }
    frame
}

/// Takes the picker's hue, saturation and value from its setting when that
/// changed under it: a preset clicked, a value typed or the file edited.
fn sync_picker(d: &mut Desktop) {
    let Some(mut picker) = d.settings.picker else {
        return;
    };
    let current = field_color(d, picker.field);
    let (h, s, v) = picker.hsv;
    if hex(current) != hex(hsv_to_color(h, s, v)) {
        picker.hsv = color_to_hsv(current);
        d.settings.picker = Some(picker);
    }
}

/// The control under a point, or `None` outside the window.
///
/// # Arguments
/// * `d` - The desktop.
/// * `x` - Pointer X.
/// * `y` - Pointer Y.
pub fn hit(d: &Desktop, x: f64, y: f64) -> Option<Hover> {
    let frame = frame(d);
    if !frame.contains(x, y) {
        return None;
    }
    let layout = layout(d);
    if let Some(picker) = layout.picker {
        for (index, control) in layout.controls.iter().enumerate().skip(picker.first) {
            if control.rect.contains(x, y) {
                return Some(Hover::Control(index));
            }
        }
        if picker.frame.contains(x, y) {
            return Some(Hover::None);
        }
    }
    if scrollbar(d).is_some_and(|bar| bar.track.contains(x, y)) {
        return Some(Hover::None);
    }
    for (index, control) in layout.controls.iter().enumerate() {
        if control.clip.is_some_and(|clip| !clip.contains(x, y)) {
            continue;
        }
        if control.rect.contains(x, y) {
            return Some(Hover::Control(index));
        }
    }
    Some(Hover::None)
}

/// Handles a click; returns whether it landed in the window.
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
    if button != Button::Left {
        return true;
    }
    let layout = layout(d);
    // A click anywhere but in the open picker closes it, then goes on to
    // whatever it landed on.
    let open = d.settings.picker.map(|picker| picker.field);
    if open.is_some() && !layout.picker.is_some_and(|picker| picker.frame.contains(x, y)) {
        d.settings.picker = None;
    }
    let Hover::Control(index) = hover else {
        commit_field(d);
        if let Some(bar) = scrollbar(d)
            && bar.track.contains(x, y)
        {
            let grab = bar.grab(y);
            d.settings.scroll = bar.scroll_at(y, grab);
            d.settings.bar_drag = Some(grab);
        }
        d.redraw_panel();
        return true;
    };
    let Some(control) = layout.controls.get(index) else {
        return true;
    };
    let action = control.action.clone();
    if !matches!(action, Action::Focus(_)) {
        commit_field(d);
        sync_picker(d);
    }
    match action {
        Action::Set(changes) => d.write_config(&changes),
        Action::Focus(field) => {
            if d.settings.focus != Some(field) {
                let text = field_text(d, field);
                d.settings.focus = Some(field);
                d.settings.editing = text;
            }
        }
        Action::Monitor(monitor) => d.settings.monitor = monitor,
        Action::Browse => {
            let key = wallpaper_key(d, "wallpaper");
            let monitor = d.settings.monitor;
            d.pick_wallpaper(key, monitor);
            return true;
        }
        Action::BrowseIcon => {
            d.pick_start_icon();
            return true;
        }
        Action::Page(page) => {
            d.settings.page = page;
            d.settings.picking = None;
            d.settings.scroll = 0.0;
        }
        Action::Pick(key) => {
            d.settings.picking = Some(key);
            d.settings.scroll = 0.0;
        }
        Action::Back => {
            d.settings.picking = None;
            d.settings.scroll = 0.0;
        }
        Action::Toggle(key, id) => d.toggle_list_entry(key, &id),
        Action::Reset => {
            let changes: Vec<(&'static str, String)> =
                page_keys(d).into_iter().map(|key| (key, String::new())).collect();
            d.write_config(&changes);
        }
        Action::Slide(key) => {
            let value = slider_at(key, control.rect, x);
            d.settings.drag = Some((key, value));
            d.preview_panel(key, value);
        }
        Action::OpenSettings | Action::Reload => {}
        Action::Close => {
            d.close_popup();
            return true;
        }
        Action::OpenPicker(field) => {
            if open != Some(field) {
                let hsv = color_to_hsv(field_color(d, field));
                d.settings.picker = Some(Picker {
                    field,
                    hsv,
                    drag: None,
                });
            }
        }
        Action::Hue => pick_hue(d, control.rect, x, true),
        Action::Area => pick_area(d, control.rect, x, y, true),
    }
    d.set_hover(hit(d, x, y).unwrap_or(Hover::None));
    d.redraw_panel();
    true
}

/// Sets the picker's hue from a pointer X over the hue strip.
///
/// # Arguments
/// * `d` - The desktop.
/// * `rect` - The hue strip.
/// * `x` - Pointer X.
/// * `start` - Whether this press starts a drag.
fn pick_hue(d: &mut Desktop, rect: Rect, x: f64, start: bool) {
    let Some(mut picker) = d.settings.picker else {
        return;
    };
    let ratio = ((x - rect.x as f64) / rect.w.max(1) as f64).clamp(0.0, 1.0) as f32;
    picker.hsv.0 = ratio * 360.0;
    if start {
        picker.drag = Some(PickerPart::Hue);
    }
    apply_picker(d, picker);
}

/// Sets the picker's saturation and value from a pointer over the square.
///
/// # Arguments
/// * `d` - The desktop.
/// * `rect` - The square.
/// * `x` - Pointer X.
/// * `y` - Pointer Y.
/// * `start` - Whether this press starts a drag.
fn pick_area(d: &mut Desktop, rect: Rect, x: f64, y: f64, start: bool) {
    let Some(mut picker) = d.settings.picker else {
        return;
    };
    picker.hsv.1 = ((x - rect.x as f64) / rect.w.max(1) as f64).clamp(0.0, 1.0) as f32;
    picker.hsv.2 = 1.0 - ((y - rect.y as f64) / rect.h.max(1) as f64).clamp(0.0, 1.0) as f32;
    if start {
        picker.drag = Some(PickerPart::Area);
    }
    apply_picker(d, picker);
}

/// Keeps a picker state and shows its color on screen without writing it.
fn apply_picker(d: &mut Desktop, picker: Picker) {
    d.settings.picker = Some(picker);
    let (h, s, v) = picker.hsv;
    preview_color(d, picker.field, hsv_to_color(h, s, v));
    d.redraw_panel();
}

/// Applies a color on screen without writing it: an interface color to the
/// theme the bar and this window draw with, the background to the monitor
/// shown.  The file written when the drag ends brings the rest along.
///
/// # Arguments
/// * `d` - The desktop.
/// * `field` - The color field.
/// * `color` - The color.
fn preview_color(d: &mut Desktop, field: Field, color: Color) {
    match field {
        Field::Theme(which) => {
            match which {
                ThemeColor::Accent => d.config.theme.accent = color,
                ThemeColor::Panel => d.config.theme.panel = color,
                ThemeColor::Popup => d.config.theme.popup = color,
                ThemeColor::Text => d.config.theme.text = color,
            }
            ui::set_theme(d.config.theme);
        }
        Field::Color => {
            match d.settings.monitor {
                None => d.config.wallpaper.color = color,
                Some(n) => d.config.monitors.entry(n).or_default().color = Some(color),
            }
            d.repaint_wallpapers();
        }
        Field::ClockFormat | Field::StartIcon | Field::StartLabel | Field::Image => {}
    }
}

/// The config key a color field writes.
fn color_key(d: &Desktop, field: Field) -> Option<&'static str> {
    match field {
        Field::Color => Some(wallpaper_key(d, "background")),
        Field::Theme(color) => Some(color.key()),
        Field::ClockFormat | Field::StartIcon | Field::StartLabel | Field::Image => None,
    }
}

/// Follows a drag of a slider or of a part of the color picker, applying the
/// value on screen.
///
/// # Arguments
/// * `d` - The desktop.
/// * `x` - Pointer X.
/// * `y` - Pointer Y.
pub fn drag(d: &mut Desktop, x: f64, y: f64) {
    if let Some(grab) = d.settings.bar_drag {
        if let Some(bar) = scrollbar(d) {
            d.settings.scroll = bar.scroll_at(y, grab);
        }
        d.redraw_panel();
        return;
    }
    if let Some(picker) = d.settings.picker
        && let Some(part) = picker.drag
    {
        let Some(rects) = layout(d).picker else {
            return;
        };
        match part {
            PickerPart::Hue => pick_hue(d, rects.hue, x, false),
            PickerPart::Area => pick_area(d, rects.area, x, y, false),
        }
        return;
    }
    let Some((key, current)) = d.settings.drag else {
        return;
    };
    let Some(rect) = slider_rect(d, key) else {
        return;
    };
    let value = slider_at(key, rect, x);
    if value != current {
        d.settings.drag = Some((key, value));
        d.preview_panel(key, value);
    }
    d.redraw_panel();
}

/// Ends a drag of a slider or of the color picker, writing the value.
///
/// # Arguments
/// * `d` - The desktop.
pub fn release(d: &mut Desktop) {
    if d.settings.bar_drag.take().is_some() {
        let (x, y) = d.pointer;
        d.set_hover(hit(d, x, y).unwrap_or(Hover::None));
        d.redraw_panel();
    }
    if let Some(mut picker) = d.settings.picker
        && picker.drag.take().is_some()
    {
        d.settings.picker = Some(picker);
        if let Some(key) = color_key(d, picker.field) {
            let (h, s, v) = picker.hsv;
            d.write_config(&[(key, hex(hsv_to_color(h, s, v)))]);
        }
    }
    if let Some((key, value)) = d.settings.drag.take() {
        d.cancel_preview();
        d.write_config(&[(key, value.to_string())]);
    }
}

/// Writes the focused field's text and drops the focus.
fn commit_field(d: &mut Desktop) {
    let Some(field) = d.settings.focus.take() else {
        return;
    };
    let text = std::mem::take(&mut d.settings.editing);
    let (key, value) = match field {
        Field::Color => match parse_color(&text) {
            Some(_) => (wallpaper_key(d, "background"), text.trim().to_string()),
            None => return,
        },
        Field::Theme(color) => match parse_color(&text) {
            Some(_) => (color.key(), text.trim().to_string()),
            None => return,
        },
        Field::Image => {
            let trimmed = text.trim();
            let value = if trimmed.is_empty() {
                "none".to_string()
            } else {
                trimmed.to_string()
            };
            (wallpaper_key(d, "wallpaper"), value)
        }
        Field::ClockFormat => ("clock_format", text),
        Field::StartIcon => ("start_icon", text.trim().to_string()),
        Field::StartLabel => ("start_label", text.trim().to_string()),
    };
    d.write_config(&[(key, value)]);
    sync_picker(d);
}

/// Handles Escape: drops the focus of a field being typed into, keeping the
/// setting as it was, else closes the color picker.
///
/// # Arguments
/// * `d` - The desktop.
///
/// # Returns
/// Whether either was open, so the key is used up.
pub fn escape(d: &mut Desktop) -> bool {
    if d.settings.focus.take().is_some() {
        d.settings.editing.clear();
    } else if d.settings.picker.take().is_none() {
        return false;
    }
    let (x, y) = d.pointer;
    d.set_hover(hit(d, x, y).unwrap_or(Hover::None));
    d.redraw_panel();
    true
}

/// Handles wheel scrolling over the page or the application list; returns
/// whether it landed in the window.
///
/// # Arguments
/// * `d` - The desktop.
/// * `x` - Pointer X.
/// * `y` - Pointer Y.
/// * `delta` - Scroll amount, positive downwards.
pub fn scroll(d: &mut Desktop, x: f64, y: f64, delta: f64) -> bool {
    let frame = frame(d);
    if !frame.contains(x, y) {
        return false;
    }
    let page = page_rect_in(frame);
    let content = build(d, frame, 0.0, view(d)).content as f64;
    let max = (content - page.h as f64).max(0.0);
    let next = (d.settings.scroll + delta.signum() * ui::SCROLL_STEP).clamp(0.0, max);
    if next != d.settings.scroll {
        d.settings.scroll = next;
        d.set_hover(hit(d, x, y).unwrap_or(Hover::None));
        d.redraw_panel();
    }
    true
}

/// Handles a key press while the window is open: typing goes to the focused
/// field, Enter writes it.
///
/// # Arguments
/// * `d` - The desktop.
/// * `key` - The key.
pub fn key(d: &mut Desktop, key: Key) {
    if d.settings.focus.is_none() {
        return;
    }
    match key {
        Key::Char(c) => d.settings.editing.push(c),
        Key::Backspace => {
            d.settings.editing.pop();
        }
        Key::Enter | Key::Tab => commit_field(d),
        _ => return,
    }
    d.redraw_panel();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A config built from a preset's own changes.
    fn config_from(changes: &[(&str, String)]) -> Config {
        let values: HashMap<String, String> = changes
            .iter()
            .map(|(key, value)| (key.to_string(), value.clone()))
            .collect();
        Config::from_values(&values)
    }

    #[test]
    fn each_style_matches_only_itself() {
        for style in &STYLES {
            let config = config_from(&style.changes());
            assert_eq!(current_style(&config), Some(style.name));
            assert_eq!(style.changes().len(), 7);
        }
        assert_eq!(current_style(&Config::default()), Some("Linux"));
        let mut odd = Config::default();
        odd.window.corners = 3;
        assert_eq!(current_style(&odd), None);
    }

    #[test]
    fn each_scheme_matches_only_itself() {
        for (name, colors) in &SCHEMES {
            let changes: Vec<(&str, String)> = COLOR_KEYS
                .iter()
                .zip(colors)
                .map(|(key, color)| (*key, color.to_string()))
                .collect();
            assert_eq!(current_scheme(config_from(&changes).theme), Some(*name));
        }
        assert_eq!(current_scheme(ui::Theme::default()), Some("Dark"));
        let theme = ui::Theme {
            accent: parse_color("#123456").unwrap(),
            ..ui::Theme::default()
        };
        assert_eq!(current_scheme(theme), None);
    }

    #[test]
    fn swatch_rows_fit_on_one_line() {
        let page = page_rect_in(Rect::new(0, 0, WIDTH, HEIGHT));
        let area = page.w - 2 * CARD_PAD - TITLE_WIDTH;
        for presets in [&SWATCHES[..], &ACCENTS[..], &SURFACES[..], &TEXTS[..]] {
            let mut widths: Vec<i32> = presets.iter().map(|_| SWATCH).collect();
            widths.push(PICKER_BUTTON);
            assert_eq!(flow(0, area, &widths).1, 1);
            for swatch in presets {
                assert!(parse_color(swatch).is_some(), "{swatch}");
            }
        }
        assert_eq!(flow(0, 100, &[60, 60]).1, 2);
    }
}
