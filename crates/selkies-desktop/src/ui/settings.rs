//! The settings window and the bar's right-click menus.
//!
//! The window is a toplevel of the shell's own, opened through the same
//! machinery as a file manager window and placed and decorated by the window
//! manager, so it floats, moves and closes like any other.  Inside it a
//! sidebar carries the pages, grouped under captions:
//! Appearance (Theme, Colors, Windows, Wallpaper), Workspace (Panel,
//! Desktops, Shortcuts), Applications (Files, Language) and System (About).
//! A caption is set small, upper case and dim under a hairline, so it reads
//! as a heading rather than a page; the page shown is a filled accent pill,
//! inset from the sidebar's edge on both sides.  The page fills
//! the rest of the window with captioned cards of rows, each row a name on
//! the left and its control on the right, and scrolls when it is taller than
//! the window.  Every change is written to the config file at once, which
//! the desktop then reads back, so the file stays the one source of the
//! settings.  Images are chosen through the file manager in its picker form,
//! applications for the quick launch and favorites lists from a list that
//! opens in place, and a color from a row of presets or from a picker with a
//! spectrum and a hex field that opens over the page and shows the color on
//! screen as it is dragged.
//!
//! The Theme and Wallpaper pages pin a live preview above their rows, which
//! stays in place while the rows scroll.  The Theme page shows each scheme
//! and each style as a card with the desktop rendered as it would look
//! under it, the one in effect ringed with the accent; the cards of a group
//! share one line when they fit.  A style also sets the bar's layout and
//! margin and the start menu's layout, which live on the Panel page.  The
//! Wallpaper page's preview is the monitor shown, painted as the desktop
//! paints it.
//!
//! The Shortcuts page edits what the keyboard does.  It lists the binds the
//! window manager publishes ([`keybinds`]), read again every second while the
//! page is shown so a change shows once the window manager applied it.  A
//! click on a bind's keys captures a new chord: the window's next key with
//! the modifiers held becomes it, a modifier alone only when it is released
//! with nothing else pressed, Escape cancels and Backspace turns the bind
//! off.  The chord is written to the user's overrides file, which the window
//! manager reads, and while the capture runs the window holds the keyboard
//! where the display server lets it, so a chord the window manager already
//! binds reaches the window instead of running.  A changed bind has a reset,
//! two binds on one chord name each other, and a row at the end adds a bind
//! that runs a command.  The shell's own fixed keys follow.
//!
//! The right-click menu of the bar depends on what was clicked
//! ([`PanelTarget`]): every item offers what hides or configures it, the
//! pager adds and removes desktops, and each "settings" entry opens the
//! window on the page concerned.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tiny_skia::{Color, PathBuilder, Pixmap};
use xkeysym::Keysym;

use crate::config::{
    self, BackgroundStyle, ButtonSide, COLOR_KEYS, Config, FILES_KEYS, Glyphs, MenuLayout, PANEL_KEYS,
    Surface, WALLPAPER_KEYS, WINDOW_KEYS, WallpaperMode,
};
use crate::desktop::{Desktop, Geometry, Hover, PanelTarget, Popup};
use crate::event::{Button, Modifiers, Request};
use crate::geom::Rect;
use crate::i18n::{self, tr, trf};
use crate::keybinds::{self, Published};
use crate::render::{Align, Canvas, color_to_hsv, hsv_to_color, parse_color};
use crate::ui::{self, popups, preview};
use crate::xkb::Key;

/// Width the settings window opens at.
pub const WIDTH: i32 = 900;
/// Height the settings window opens at.
pub const HEIGHT: i32 = 600;
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
/// Width of the sidebar, its pages inset from both of its edges.
const SIDEBAR: i32 = 220;
/// Height of a page row in the sidebar.
const NAV_ROW: i32 = 32;
/// Height of a group caption in the sidebar.
const NAV_CAPTION: i32 = 22;
/// Space above a sidebar group after the first, its hairline in the middle.
const NAV_GROUP_GAP: i32 = 17;
/// Font size of a sidebar group caption.
const NAV_CAPTION_FONT: f32 = 10.5;
/// Side of an icon chip.
const CHIP: i32 = 30;
/// Height of an application list row.
const LIST_ROW: i32 = 30;
/// Width of the right-click menu.
const MENU_WIDTH: i32 = 250;
/// Row height of the right-click menu.
const MENU_ROW: i32 = 30;
/// Height of a separator in the right-click menu.
const MENU_SEPARATOR: i32 = 9;
/// Space between controls flowed in a row.
const GAP: i32 = 4;
/// Width of a toggle switch.
pub const SWITCH_WIDTH: i32 = 42;
/// Height of a toggle switch.
pub const SWITCH_HEIGHT: i32 = 22;
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
/// The least width of a preset card; a group's cards share its line when
/// they fit at this width.
const PRESET_MIN_WIDTH: i32 = 110;
/// The width under which a preset card names itself in the small font.
const PRESET_SMALL_NAME: i32 = 150;
/// Inner padding of a preset card.
const PRESET_PAD: i32 = 8;
/// Height of the name line under a preset's preview.
const PRESET_NAME: i32 = 22;
/// Narrowest a wallpaper card gets.
const WALLPAPER_CARD_MIN: i32 = 150;
/// Height of the author line under a wallpaper card's name.
const WALLPAPER_AUTHOR: i32 = 16;
/// Space between preset cards.
const PRESET_GAP: i32 = 10;
/// Height of a key cap on the Shortcuts page.
const KEYCAP: i32 = 24;
/// Width of the slash between alternative chords.
const KEYCAP_SLASH: i32 = 12;
/// Height a shortcut row gains for the line under its description.
const SHORTCUT_NOTE: i32 = 14;
/// Height of a note line at the head of a page.
const NOTE_LINE: i32 = 18;
/// How often the published shortcuts are read again while the Shortcuts
/// page is shown.
const KEYBINDS_POLL: Duration = Duration::from_secs(1);
/// Side of the mark beside a warning.
const WARNING_MARK: i32 = 8;
/// Thickness of an accent mark, which the video stream carries at half
/// resolution in chroma and so blurs when thinner.
const ACCENT_STROKE: f32 = 3.0;

/// The sliders of the Panel, Desktops and Windows pages: key, lowest,
/// highest, step and unit.
const SLIDERS: [(&str, i32, i32, i32, &str); 7] = [
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
    ("panel_corners", 0, config::MAX_PANEL_CORNERS, 1, "px"),
    ("window_corners", 0, config::MAX_CORNERS, 1, "px"),
    ("desktops", 1, config::MAX_DESKTOPS as i32, 1, ""),
];

/// The keys of the Desktops page, which the Panel page's reset leaves alone.
const DESKTOP_KEYS: [&str; 3] = ["desktops", "show_pager", "tasks_all_desktops"];

/// The languages the interface comes in, by code, each named in itself.
const LANGUAGES: [(&str, &str); 19] = [
    ("en", "English"),
    ("ar", "العربية"),
    ("da", "Dansk"),
    ("de", "Deutsch"),
    ("es", "Español"),
    ("fil", "Filipino"),
    ("fr", "Français"),
    ("hi", "हिन्दी"),
    ("it", "Italiano"),
    ("ja", "日本語"),
    ("ko", "한국어"),
    ("nl", "Nederlands"),
    ("pt", "Português"),
    ("ru", "Русский"),
    ("th", "ไทย"),
    ("tr", "Türkçe"),
    ("vi", "Tiếng Việt"),
    ("zh_cn", "简体中文"),
    ("zh_tw", "繁體中文"),
];

/// The color schemes: the four colors of each, as accent, bar, menus and
/// text.  Dark, the default, and Light share a sky-blue accent on neutral
/// grays, the bar the shade of a window's header and the menus a window's
/// own; Ember is a copper accent on warm charcoal, dark enough to take
/// white text; Mint is a green accent on pale sage, a light desktop of a
/// warmer cast than Light; and Graphite is a rose accent on near black, a
/// darker desktop than Dark.
const SCHEMES: [(&str, [&str; 4]); 5] = [
    ("Dark", ["#3daee9", "#292c30", "#202326", "#fcfcfc"]),
    ("Light", ["#3daee9", "#dee0e2", "#eff0f1", "#232629"]),
    ("Ember", ["#b9671b", "#1c1917", "#272220", "#f4ede4"]),
    ("Mint", ["#13866a", "#d8e9e2", "#f2f8f5", "#14302a"]),
    ("Graphite", ["#e8557a", "#0c0c0e", "#16161a", "#f1f1f4"]),
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
    bar_corners: i32,
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
            && config.panel_corners == self.bar_corners
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
            ("panel_corners", self.bar_corners.to_string()),
            ("menu_layout", menu_value(self.menu).to_string()),
        ]
    }

    /// The look the desktop would have under the style, with the colors as
    /// they are.
    fn look(&self, d: &Desktop) -> preview::Look {
        let mut look = preview::Look::current(d);
        look.style.surface = self.surface;
        look.style.buttons = self.buttons;
        look.style.glyphs = self.glyphs;
        look.style.corners = self.corners;
        look.centered = self.centered;
        if d.config.panel_float {
            look.margin = self.margin;
        }
        look.bar_corners = self.bar_corners;
        look.menu = self.menu;
        look
    }
}

/// The styles: the Linux desktop as it is, the Fruit look with a rounder
/// floating dock, circle buttons and a grid of applications, and the
/// Redmond look with square frames, a square bar, wide buttons and a tiled
/// start menu. Each floats its bar by the same margin while the bar's
/// float setting is on.
const STYLES: [Style; 3] = [
    Style {
        name: "Linux",
        surface: Surface::Flat,
        buttons: ButtonSide::Right,
        glyphs: Glyphs::Lines,
        corners: 8,
        centered: false,
        margin: 8,
        bar_corners: 6,
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
        bar_corners: 14,
        menu: MenuLayout::Grid,
    },
    Style {
        name: "Redmond",
        surface: Surface::Flat,
        buttons: ButtonSide::Right,
        glyphs: Glyphs::Wide,
        corners: 0,
        centered: false,
        margin: 8,
        bar_corners: 0,
        menu: MenuLayout::Tiles,
    },
];

/// Space above and below a preview inside its card.
const PREVIEW_PAD: i32 = 10;
/// Height of the Wallpaper page's preview.
const WALLPAPER_PREVIEW: i32 = 190;
/// The mark beside a warning, an amber that reads on dark and light cards.
fn warning() -> Color {
    Color::from_rgba8(0xf0, 0xa0, 0x30, 0xff)
}

/// Preset background colors, true black first.
const SWATCHES: [&str; 12] = [
    "#000000", "#101418", "#1d222b", "#2b2b2b", "#1e2a3a", "#243b2f", "#3b2d4a", "#5a5a5a", "#0b1f3a",
    "#2d1b33", "#4a2b1a", "#1a1a2e",
];
/// Preset accent colors, the default first.
const ACCENTS: [&str; 12] = [
    "#3daee9", "#7967c5", "#3d7dd6", "#2f9e6e", "#d5499a", "#e0803a", "#d64545", "#8a8f99", "#1fa8b8",
    "#c9a227", "#9d8cf0", "#b56576",
];
/// Preset surface colors for the bar and the menus, the defaults first and
/// the light ones last.
const SURFACES: [&str; 12] = [
    "#292c30", "#202326", "#0a0e17", "#121826", "#000000", "#1d222b", "#2e3440", "#3b2d4a", "#dee0e2",
    "#eff0f1", "#fcfcfc", "#fdf6e3",
];
/// Preset text colors, the default first and the dark ones last.
const TEXTS: [&str; 12] = [
    "#fcfcfc", "#eef2f8", "#ffffff", "#d0d4da", "#b6c0d2", "#f5e6c8", "#c8e6ff", "#93a1a1", "#586e75",
    "#333333", "#232629", "#111111",
];

/// The groups the sidebar lists the pages under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    /// How the desktop looks.
    Appearance,
    /// The bar, the desktops and the keys.
    Workspace,
    /// The file manager and the language.
    Applications,
    /// About the shell.
    System,
}

impl Group {
    /// Every group in sidebar order.
    const ALL: [Group; 4] = [
        Group::Appearance,
        Group::Workspace,
        Group::Applications,
        Group::System,
    ];

    /// The group's caption.
    fn name(self) -> &'static str {
        match self {
            Group::Appearance => tr("Appearance"),
            Group::Workspace => tr("Workspace"),
            Group::Applications => tr("Applications"),
            Group::System => tr("System"),
        }
    }
}

/// The pages of the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    /// The scheme and style presets, with the live preview.
    Theme,
    /// The four colors of the interface.
    Colors,
    /// The window frames.
    Windows,
    /// The wallpaper of every monitor.
    Wallpaper,
    /// The bar and the start menu.
    Panel,
    /// The virtual desktops.
    Desktops,
    /// The keyboard shortcuts in force.
    Shortcuts,
    /// The file manager.
    Files,
    /// The interface language.
    Language,
    /// The shell's version and session.
    About,
}

impl Page {
    /// Every page in sidebar order.
    pub const ALL: [Page; 10] = [
        Page::Theme,
        Page::Colors,
        Page::Windows,
        Page::Wallpaper,
        Page::Panel,
        Page::Desktops,
        Page::Shortcuts,
        Page::Files,
        Page::Language,
        Page::About,
    ];

    /// The page's id, as `--settings <page>` names it.
    pub fn id(self) -> &'static str {
        match self {
            Page::Theme => "theme",
            Page::Colors => "colors",
            Page::Windows => "windows",
            Page::Wallpaper => "wallpaper",
            Page::Panel => "panel",
            Page::Desktops => "desktops",
            Page::Shortcuts => "shortcuts",
            Page::Files => "files",
            Page::Language => "language",
            Page::About => "about",
        }
    }

    /// The page with an id, case and surrounding space ignored.
    pub fn from_id(id: &str) -> Option<Page> {
        let id = id.trim().to_ascii_lowercase();
        Page::ALL.into_iter().find(|page| page.id() == id)
    }

    /// The page's name.
    fn name(self) -> &'static str {
        match self {
            Page::Theme => tr("Theme"),
            Page::Colors => tr("Colors"),
            Page::Windows => tr("Windows"),
            Page::Wallpaper => tr("Wallpaper"),
            Page::Panel => tr("Panel"),
            Page::Desktops => tr("Desktops"),
            Page::Shortcuts => tr("Shortcuts"),
            Page::Files => tr("Files"),
            Page::Language => tr("Language"),
            Page::About => tr("About"),
        }
    }

    /// The group the sidebar lists the page under.
    fn group(self) -> Group {
        match self {
            Page::Theme | Page::Colors | Page::Windows | Page::Wallpaper => Group::Appearance,
            Page::Panel | Page::Desktops | Page::Shortcuts => Group::Workspace,
            Page::Files | Page::Language => Group::Applications,
            Page::About => Group::System,
        }
    }
}

/// The shell's own fixed shortcuts: the chord and what it does.
fn desktop_shortcuts() -> Vec<(&'static str, &'static str)> {
    vec![
        ("Super", tr("Quick launcher")),
        ("Escape", tr("Close the popup")),
        ("Up / Down / Tab", tr("Move through the launcher's results")),
        ("Enter", tr("Launch the selected result")),
        ("A / Z", tr("Type to search applications and commands")),
    ]
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
    /// The command of a shortcut being added.
    Command,
}

/// What a captured chord is for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureTarget {
    /// A bind of the window manager's configuration, by the chord the
    /// configuration gives it.
    Rebind(String),
    /// A bind of the user's that runs a command, by its chord and command.
    Custom { spec: String, command: String },
    /// The bind being added for the command typed.
    Add,
}

/// A shortcut being captured from the keyboard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capture {
    /// What the chord is for.
    pub target: CaptureTarget,
    /// A modifier pressed while nothing else was held, which becomes the
    /// chord itself if it is released before anything else is pressed.
    held: Option<u32>,
}

/// What clicking a control or a menu item does.
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
    /// Open the settings window, on a page or as it was, from the bar's menu.
    OpenSettings(Option<Page>),
    /// Open the settings window on the favorites list, from the bar's menu.
    EditFavorites,
    /// Read the applications again, from the bar's menu.
    Reload,
    /// Toggle mute, from the volume control's menu.
    ToggleMute,
    /// Take an application off the quick launch, from its button's menu.
    RemoveLauncher(String),
    /// Turn Do not disturb on or off, from the bell's menu.
    ToggleDnd,
    /// Remove every notification, from the bell's menu.
    ClearNotifications,
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
    /// Capture a chord for a shortcut from the keyboard.
    Capture(CaptureTarget),
    /// Put a bind back on the chord its configuration gives it, by that
    /// chord.
    ResetBind(String),
    /// Remove a bind of the user's that runs a command.
    RemoveCustom { spec: String, command: String },
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
    /// A page in the sidebar.
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
    /// A preset card: a rendered preview of its look over its name.
    Preset,
    /// A shortcut's chord as key caps, which a click captures anew; the
    /// label is the chord's label, empty for a bind turned off.
    Keys,
    /// A wallpaper card: its thumbnail, split into its light and dark
    /// pictures where it has both, over its name and author.
    Wallpaper,
}

/// What a wallpaper card shows.
#[derive(Clone, Debug, PartialEq)]
struct WallpaperCard {
    /// The light picture, or `None` for the application icon on the wallpaper's color.
    light: Option<PathBuf>,
    /// The dark picture, drawn on the card's right half, where the wallpaper has one.
    dark: Option<PathBuf>,
    /// Who made it.
    author: Option<String>,
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
    /// The look a preset card shows.
    look: Option<preview::Look>,
    /// What a wallpaper card shows.
    wallpaper: Option<WallpaperCard>,
    /// The area the control is confined to when it scrolls.
    clip: Option<Rect>,
}

/// How a piece of text in the page is set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tone {
    /// A row's name.
    Name,
    /// A group's caption, bold and dim.
    Caption,
    /// A value at a row's right, dim and flush right.
    Value,
    /// A line under a row's name, small and dim.
    Hint,
    /// A line under a row's name warning of something, small beside a mark.
    Warning,
    /// A paragraph at the head of a page, dim and wrapped over two lines.
    Note,
}

/// A piece of text in the page.
struct Text {
    rect: Rect,
    text: String,
    tone: Tone,
}

/// A key cap on the Shortcuts page, or the slash between alternatives.
struct Cap {
    rect: Rect,
    text: String,
    /// Whether the text is a key rather than the slash.
    key: bool,
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
    /// The group captions of the sidebar.
    nav_texts: Vec<Text>,
    /// The hairlines above the sidebar's groups.
    nav_lines: Vec<Rect>,
    /// Card backgrounds.
    cards: Vec<Rect>,
    /// Hairlines between rows.
    lines: Vec<Rect>,
    /// The card of the preview pinned above the page's rows, when the page
    /// has one.
    pinned: Option<Rect>,
    /// The key caps of the Shortcuts page.
    caps: Vec<Cap>,
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

/// The window's state between events, and the window itself while open.
pub struct SettingsState {
    /// The desktop's number for the window while it is open.
    pub id: Option<u32>,
    /// Size and scale of the window.
    pub geometry: Geometry,
    /// The rendered pixels.
    pub pix: Option<Pixmap>,
    /// Whether the window needs drawing.
    pub dirty: bool,
    /// Last pointer position in the window.
    pub pointer: (f64, f64),
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
    /// The window manager's shortcuts as last read; `None` when none are
    /// published.
    pub keybinds: Option<Published>,
    /// When the published shortcuts are read next while the Shortcuts page
    /// is shown.
    poll_at: Option<Instant>,
    /// The shortcut being captured from the keyboard, if any.
    pub capture: Option<Capture>,
    /// The command of the shortcut being added.
    pub command: String,
    /// The preset cards' previews, by the look and scale they were rendered
    /// for.
    presets: HashMap<String, Arc<Pixmap>>,
    /// The wallpapers installed, read when the window opens and when the
    /// Wallpaper page is shown.
    pub wallpapers: Vec<crate::wallpapers::WallpaperEntry>,
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
            id: None,
            geometry: Geometry::default(),
            pix: None,
            dirty: false,
            pointer: (-1.0, -1.0),
            page: Page::Theme,
            picking: None,
            monitor: None,
            focus: None,
            editing: String::new(),
            scroll: 0.0,
            drag: None,
            picker: None,
            bar_drag: None,
            keybinds: None,
            poll_at: None,
            capture: None,
            command: String::new(),
            presets: HashMap::new(),
            wallpapers: Vec::new(),
        }
    }
}

impl SettingsState {
    /// Resets the window for opening, on a page when one is asked for.
    ///
    /// # Arguments
    /// * `page` - The page to show, or `None` to keep the last one.
    pub fn open(&mut self, page: Option<Page>) {
        if let Some(page) = page {
            self.page = page;
        }
        self.picking = None;
        self.monitor = None;
        self.focus = None;
        self.editing.clear();
        self.scroll = 0.0;
        self.drag = None;
        self.picker = None;
        self.bar_drag = None;
        self.pointer = (-1.0, -1.0);
        self.keybinds = keybinds::read_published();
        self.poll_at = Some(Instant::now() + KEYBINDS_POLL);
        self.capture = None;
        self.command.clear();
        self.wallpapers = crate::wallpapers::list();
        self.dirty = true;
    }

    /// Clears the state when the window closes.
    pub fn close(&mut self) {
        self.id = None;
        self.geometry = Geometry::default();
        self.pix = None;
        self.focus = None;
        self.picking = None;
        self.editing.clear();
        self.drag = None;
        self.picker = None;
        self.bar_drag = None;
        self.capture = None;
        self.poll_at = None;
    }

    /// Whether the window shows the Shortcuts page, whose shortcuts are
    /// read again every second.
    fn polls_keybinds(&self) -> bool {
        self.id.is_some() && self.page == Page::Shortcuts && self.picking.is_none()
    }

    /// When the published shortcuts are next read, while the window shows
    /// the Shortcuts page.
    pub fn next_poll(&self) -> Option<Instant> {
        self.poll_at.filter(|_| self.polls_keybinds())
    }

    /// Reads the published shortcuts again, marking the window for drawing
    /// when they changed.
    fn reread_keybinds(&mut self) {
        let fresh = keybinds::read_published();
        if fresh != self.keybinds {
            self.keybinds = fresh;
            self.dirty = true;
        }
    }

    /// Whether a slider, a part of the color picker or the scrollbar is
    /// being dragged.
    pub fn dragging(&self) -> bool {
        self.drag.is_some()
            || self.bar_drag.is_some()
            || self.picker.is_some_and(|picker| picker.drag.is_some())
    }
}

/// An entry of the bar's right-click menu.
#[derive(Clone, Debug, PartialEq)]
enum MenuEntry {
    /// A row: its label, what it does, and whether it can be chosen.
    Item {
        label: &'static str,
        action: Action,
        enabled: bool,
    },
    /// A hairline between rows.
    Separator,
}

impl MenuEntry {
    /// An enabled row.
    fn item(label: &'static str, action: Action) -> MenuEntry {
        MenuEntry::Item {
            label,
            action,
            enabled: true,
        }
    }

    /// The entry's height.
    fn height(&self) -> i32 {
        match self {
            MenuEntry::Item { .. } => MENU_ROW,
            MenuEntry::Separator => MENU_SEPARATOR,
        }
    }
}

/// What the bar's right-click menu decides its entries by.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct MenuFacts {
    /// Whether the bar sits along the top edge.
    top: bool,
    /// Whether the bar hides.
    autohide: bool,
    /// How many virtual desktops there are.
    desktops: u32,
    /// Whether the sink is muted, or `None` while audio is unavailable.
    muted: Option<bool>,
    /// The desktop file ID of the quick launch button clicked, if one was.
    launcher: Option<String>,
    /// Whether Do not disturb is on.
    dnd: bool,
    /// Whether there are notifications to clear.
    notifications: bool,
}

/// A menu entry that writes one key.
fn set_entry(label: &'static str, key: &'static str, value: &str) -> MenuEntry {
    MenuEntry::item(label, Action::Set(vec![(key, value.to_string())]))
}

/// The entries of the bar's right-click menu for what was clicked.
///
/// # Arguments
/// * `target` - The bar item clicked.
/// * `facts` - The state the entries depend on.
fn panel_menu_entries(target: PanelTarget, facts: &MenuFacts) -> Vec<MenuEntry> {
    let settings = |label: &'static str, page: Page| MenuEntry::item(label, Action::OpenSettings(Some(page)));
    let more = facts.desktops.saturating_add(1).to_string();
    let fewer = facts.desktops.saturating_sub(1).max(1).to_string();
    let add_desktop = MenuEntry::Item {
        label: tr("Add a desktop"),
        action: Action::Set(vec![("desktops", more)]),
        enabled: facts.desktops < config::MAX_DESKTOPS,
    };
    match target {
        PanelTarget::Bar => vec![
            MenuEntry::item(tr("Settings…"), Action::OpenSettings(None)),
            set_entry(
                if facts.top {
                    tr("Move panel to bottom")
                } else {
                    tr("Move panel to top")
                },
                "panel_position",
                if facts.top { "bottom" } else { "top" },
            ),
            set_entry(
                if facts.autohide {
                    tr("Always show the panel")
                } else {
                    tr("Auto-hide the panel")
                },
                "panel_autohide",
                if facts.autohide { "false" } else { "true" },
            ),
            add_desktop,
            MenuEntry::item(tr("Reload applications"), Action::Reload),
        ],
        PanelTarget::Pager => vec![
            add_desktop,
            MenuEntry::Item {
                label: tr("Remove a desktop"),
                action: Action::Set(vec![("desktops", fewer)]),
                enabled: facts.desktops > 1,
            },
            MenuEntry::Separator,
            settings(tr("Desktop settings…"), Page::Desktops),
        ],
        PanelTarget::Clock => vec![
            settings(tr("Clock settings…"), Page::Panel),
            set_entry(tr("Hide the clock"), "show_clock", "false"),
        ],
        PanelTarget::Volume => vec![
            MenuEntry::Item {
                label: if facts.muted == Some(true) {
                    tr("Unmute")
                } else {
                    tr("Mute")
                },
                action: Action::ToggleMute,
                enabled: facts.muted.is_some(),
            },
            set_entry(tr("Hide the volume control"), "show_volume", "false"),
            settings(tr("Sound settings…"), Page::Panel),
        ],
        PanelTarget::Start | PanelTarget::Launcher(_) => {
            let mut entries = vec![
                settings(tr("Panel settings…"), Page::Panel),
                MenuEntry::item(tr("Edit favorites…"), Action::EditFavorites),
            ];
            if let Some(id) = &facts.launcher {
                entries.push(MenuEntry::item(
                    tr("Remove from quick launch"),
                    Action::RemoveLauncher(id.clone()),
                ));
            }
            entries
        }
        PanelTarget::Tray => vec![
            set_entry(tr("Hide the system tray"), "show_tray", "false"),
            settings(tr("Panel settings…"), Page::Panel),
        ],
        PanelTarget::ShowDesktop => vec![
            set_entry(tr("Hide this button"), "show_desktop_button", "false"),
            settings(tr("Panel settings…"), Page::Panel),
        ],
        PanelTarget::Notifications => vec![
            MenuEntry::item(
                if facts.dnd {
                    tr("Allow notifications")
                } else {
                    tr("Do not disturb")
                },
                Action::ToggleDnd,
            ),
            MenuEntry::Item {
                label: tr("Clear all"),
                action: Action::ClearNotifications,
                enabled: facts.notifications,
            },
        ],
    }
}

/// The entries of the open right-click menu.
fn panel_menu_items(d: &Desktop) -> Vec<MenuEntry> {
    let Popup::PanelMenu(_, target) = d.popup else {
        return Vec::new();
    };
    // The bar numbers its quick launch buttons over the entries that resolve
    // to an application, as its layout does.
    let launcher = match target {
        PanelTarget::Launcher(position) => d
            .config
            .launchers
            .iter()
            .filter(|id| d.apps.find(id).is_some())
            .nth(position)
            .cloned(),
        _ => None,
    };
    let facts = MenuFacts {
        top: d.panel_top(),
        autohide: d.config.panel_autohide,
        desktops: d.desktops,
        muted: d.volume.map(|v| v.muted),
        launcher,
        dnd: d.notifications.dnd,
        notifications: !d.notifications.is_empty(),
    };
    panel_menu_entries(target, &facts)
}

/// The rectangle of the right-click menu anchored at a bar X.
fn panel_menu_rect(d: &Desktop, x: i32) -> Rect {
    let height: i32 = panel_menu_items(d).iter().map(MenuEntry::height).sum();
    let anchor = Rect::new(x - 8, d.bar_rect().y, 16, d.bar_rect().h);
    popups::anchor_above(d, anchor, MENU_WIDTH, height + 12)
}

/// The rectangles of the menu's entries, in order.
fn panel_menu_rows(d: &Desktop, frame: Rect) -> Vec<Rect> {
    let mut y = frame.y + 6;
    panel_menu_items(d)
        .iter()
        .map(|entry| {
            let rect = Rect::new(frame.x + 6, y, frame.w - 12, entry.height());
            y += entry.height();
            rect
        })
        .collect()
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
    let rows = panel_menu_rows(d, frame);
    for (row, (entry, rect)) in panel_menu_items(d).into_iter().zip(rows).enumerate() {
        let MenuEntry::Item { label, enabled, .. } = entry else {
            canvas.fill_rect(
                Rect::new(rect.x + 8, rect.y + rect.h / 2, rect.w - 16, 1),
                ui::separator(),
            );
            continue;
        };
        if enabled && d.hover == Hover::PanelMenuItem(row) {
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
            if enabled { ui::text() } else { ui::text_dim() },
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
    let Popup::PanelMenu(at, _) = d.popup else {
        return None;
    };
    let frame = panel_menu_rect(d, at);
    if !frame.contains(x, y) {
        return None;
    }
    let rows = panel_menu_rows(d, frame);
    for (row, (entry, rect)) in panel_menu_items(d).iter().zip(rows).enumerate() {
        if rect.contains(x, y) {
            return match entry {
                MenuEntry::Item { enabled: true, .. } => Some(Hover::PanelMenuItem(row)),
                _ => Some(Hover::None),
            };
        }
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
        && let Some(MenuEntry::Item { action, .. }) = panel_menu_items(d).get(row).cloned()
    {
        d.close_popup();
        match action {
            Action::OpenSettings(page) => d.open_settings(page),
            Action::EditFavorites => {
                d.open_settings(Some(Page::Panel));
                d.settings.picking = Some("favorites");
            }
            Action::Set(changes) => d.write_config(&changes),
            Action::ToggleMute => d.toggle_mute(),
            Action::RemoveLauncher(id) => d.toggle_list_entry("launchers", &id),
            Action::ToggleDnd => d.set_dnd(!d.notifications.dnd),
            Action::ClearNotifications => d.clear_notifications(),
            _ => d.reload(),
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

/// The window's rectangle: the whole of its surface.
fn frame(d: &Desktop) -> Rect {
    Rect::new(0, 0, d.settings.geometry.width, d.settings.geometry.height)
}

/// The sidebar's column of pages, for a frame: inset by the same margin from the window's edge
/// on the left and from the sidebar's divider on the right.
fn sidebar_rect_in(frame: Rect) -> Rect {
    Rect::new(frame.x + PAD, frame.y + PAD, SIDEBAR - 2 * PAD, frame.h - 2 * PAD)
}

/// The page area to the right of the sidebar, for a frame: above the
/// footer with the Reset or Back button, and short of the gutter its
/// scrollbar sits in.
fn page_rect_in(frame: Rect) -> Rect {
    let x = frame.x + SIDEBAR + PAD;
    let y = frame.y + PAD;
    let bottom = frame.bottom() - PAD - CONTROL - 10;
    Rect::new(x, y, frame.right() - PAD - ui::SCROLLBAR - x, bottom - y)
}

/// Height of what a view pins above its scrolled rows, with the gap under
/// it: the Theme and Wallpaper pages' previews.
fn pinned_height(view: View) -> i32 {
    match view {
        View::Page(Page::Theme) => preview::HEIGHT + 2 * PREVIEW_PAD + GROUP_GAP,
        View::Page(Page::Wallpaper) => WALLPAPER_PREVIEW + 2 * PREVIEW_PAD + GROUP_GAP,
        _ => 0,
    }
}

/// The part of the page area that scrolls, for a frame: the page area
/// below what the view pins.
fn scroll_rect_in(frame: Rect, view: View) -> Rect {
    let page = page_rect_in(frame);
    let top = pinned_height(view).min(page.h);
    Rect::new(page.x, page.y + top, page.w, page.h - top)
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
        Field::Command => d.settings.command.clone(),
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
        Field::ClockFormat | Field::StartIcon | Field::StartLabel | Field::Image | Field::Command => {
            Color::BLACK
        }
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

/// The keys a page's reset removes; none for a page that holds no setting.
fn page_keys(d: &Desktop) -> Vec<&'static str> {
    match d.settings.page {
        Page::Theme => COLOR_KEYS.iter().chain(WINDOW_KEYS.iter()).copied().collect(),
        Page::Colors => COLOR_KEYS.to_vec(),
        Page::Windows => WINDOW_KEYS.to_vec(),
        Page::Wallpaper => {
            let mut keys: Vec<&'static str> = WALLPAPER_KEYS.iter().map(|k| wallpaper_key(d, k)).collect();
            if d.settings.monitor.is_none() {
                keys.push("wallpaper_variant");
            }
            keys
        }
        Page::Panel => PANEL_KEYS
            .iter()
            .copied()
            .filter(|key| !DESKTOP_KEYS.contains(key))
            .collect(),
        Page::Desktops => DESKTOP_KEYS.to_vec(),
        Page::Files => FILES_KEYS.to_vec(),
        Page::Language => vec!["language"],
        Page::Shortcuts | Page::About => Vec::new(),
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

/// The theme a scheme's four colors make.
fn scheme_theme(colors: &[&str; 4]) -> ui::Theme {
    let mut theme = ui::Theme::default();
    let mut slots = [
        &mut theme.accent,
        &mut theme.panel,
        &mut theme.popup,
        &mut theme.text,
    ];
    for (slot, color) in slots.iter_mut().zip(colors) {
        if let Some(parsed) = parse_color(color) {
            **slot = parsed;
        }
    }
    theme
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
        "Ember" => tr("Ember"),
        "Mint" => tr("Mint"),
        "Graphite" => tr("Graphite"),
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

/// A preset card to lay out: its name, the look it shows, what choosing it
/// writes, and whether it is in effect.
struct PresetCard {
    name: String,
    look: preview::Look,
    action: Action,
    active: bool,
}

/// A wallpaper card to lay out: its name, what it shows, what choosing it
/// writes, and whether it is the one shown.
struct WallpaperChoice {
    name: String,
    card: WallpaperCard,
    action: Action,
    active: bool,
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
            tone: Tone::Caption,
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

    /// Opens a card without a caption when none is open, and counts a row
    /// in it, with a hairline above when it is not the first.
    fn begin_row(&mut self) {
        if self.card_top.is_none() {
            self.card_top = Some(self.y);
            self.rows = 0;
        }
        if self.rows > 0 {
            self.out
                .lines
                .push(Rect::new(self.x + CARD_PAD, self.y, self.width - 2 * CARD_PAD, 1));
        }
        self.rows += 1;
    }

    /// Starts a row with a name; returns the area its control goes in.
    ///
    /// # Arguments
    /// * `title` - The row's name.
    /// * `lines` - Lines of `ROW` height the control takes.
    fn row(&mut self, title: &str, lines: i32) -> Rect {
        self.begin_row();
        let inner_x = self.x + CARD_PAD;
        let inner_w = self.width - 2 * CARD_PAD;
        let height = lines.max(1) * ROW;
        if !title.is_empty() {
            self.out.texts.push(Text {
                rect: Rect::new(inner_x, self.y, TITLE_WIDTH - 8, ROW),
                text: title.to_string(),
                tone: Tone::Name,
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
            look: None,
            wallpaper: None,
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

    /// Adds the preset cards of a group: on one line, sharing its width,
    /// when they fit there at the least width, else as many a line as fit.
    /// A card is its look's preview at the preview's aspect over its name.
    fn presets(&mut self, cards: Vec<PresetCard>) {
        self.begin_row();
        let inner_x = self.x + CARD_PAD;
        let inner_w = self.width - 2 * CARD_PAD;
        let fit = ((inner_w + PRESET_GAP) / (PRESET_MIN_WIDTH + PRESET_GAP)).max(1) as usize;
        let columns = cards.len().clamp(1, fit);
        let width = (inner_w - (columns as i32 - 1) * PRESET_GAP) / columns as i32;
        let image_h = (width - 2 * PRESET_PAD) * preview::HEIGHT / preview::WIDTH;
        let height = image_h + 2 * PRESET_PAD + PRESET_NAME;
        let lines = cards.len().div_ceil(columns).max(1) as i32;
        for (i, card) in cards.into_iter().enumerate() {
            let (col, line) = ((i % columns) as i32, (i / columns) as i32);
            let rect = Rect::new(
                inner_x + col * (width + PRESET_GAP),
                self.y + PRESET_GAP + line * (height + PRESET_GAP),
                width,
                height,
            );
            self.push(Kind::Preset, rect, card.action, &card.name, card.active)
                .look = Some(card.look);
        }
        self.y += PRESET_GAP + lines * (height + PRESET_GAP);
    }

    /// Adds the wallpaper cards of a group, as many a line as fit at the
    /// least width sharing it: a card is its thumbnail at a screen's shape
    /// over its name and its author.
    fn wallpapers(&mut self, cards: Vec<WallpaperChoice>) {
        self.begin_row();
        let inner_x = self.x + CARD_PAD;
        let inner_w = self.width - 2 * CARD_PAD;
        let columns = ((inner_w + PRESET_GAP) / (WALLPAPER_CARD_MIN + PRESET_GAP)).max(1) as usize;
        let width = (inner_w - (columns as i32 - 1) * PRESET_GAP) / columns as i32;
        let image_h = (width - 2 * PRESET_PAD) * 9 / 16;
        let height = image_h + 2 * PRESET_PAD + PRESET_NAME + WALLPAPER_AUTHOR;
        let lines = cards.len().div_ceil(columns).max(1) as i32;
        for (i, choice) in cards.into_iter().enumerate() {
            let (col, line) = ((i % columns) as i32, (i / columns) as i32);
            let rect = Rect::new(
                inner_x + col * (width + PRESET_GAP),
                self.y + PRESET_GAP + line * (height + PRESET_GAP),
                width,
                height,
            );
            self.push(Kind::Wallpaper, rect, choice.action, &choice.name, choice.active)
                .wallpaper = Some(choice.card);
        }
        self.y += PRESET_GAP + lines * (height + PRESET_GAP);
    }

    /// Adds a paragraph at the head of the page, outside any card, over two
    /// lines.
    fn note(&mut self, text: &str) {
        self.finish();
        self.started = true;
        self.out.texts.push(Text {
            rect: Rect::new(
                self.x + CARD_PAD,
                self.y,
                self.width - 2 * CARD_PAD,
                2 * NOTE_LINE,
            ),
            text: text.to_string(),
            tone: Tone::Note,
        });
        self.y += 2 * NOTE_LINE;
    }

    /// Adds a row with one flat button at its right.
    fn button(&mut self, title: &str, label: &str, action: Action) {
        let area = self.row(title, 1);
        let w = chip_width(label).min(area.w);
        let rect = Rect::new(area.right() - w, area.y + (ROW - CONTROL) / 2, w, CONTROL);
        self.push(Kind::Button, rect, action, label, false);
    }

    /// Adds a row showing a value at its right, which nothing changes; a row
    /// without a value gives its name the whole width.
    fn info(&mut self, title: &str, value: &str) {
        let area = self.row(title, 1);
        if value.is_empty() {
            if !title.is_empty()
                && let Some(name) = self.out.texts.last_mut()
            {
                name.rect.w = area.right() - name.rect.x;
            }
            return;
        }
        self.out.texts.push(Text {
            rect: area,
            text: value.to_string(),
            tone: Tone::Value,
        });
    }

    /// Adds a shortcut row: the description at the left over the width the
    /// chord leaves it, a line under it when the row has one, and at the
    /// right the chord's keys as caps, alternatives separated by a slash,
    /// with the row's buttons before them.  A row that can be captured
    /// anew has its caps as one control, which shows the prompt while it
    /// captures.
    fn shortcut(&mut self, row: ShortcutRow<'_>) {
        self.begin_row();
        let inner_x = self.x + CARD_PAD;
        let inner_w = self.width - 2 * CARD_PAD;
        let height = if row.note.is_some() {
            ROW + SHORTCUT_NOTE
        } else {
            ROW
        };
        let cap_y = self.y + (height - KEYCAP) / 2;
        let mut right = inner_x + inner_w;
        match &row.capture {
            Some(target) => {
                let capturing = row.capturing;
                let width = if capturing {
                    chip_width(tr("Press a key combination…"))
                } else if row.chord.is_empty() {
                    chip_width(tr("Off"))
                } else {
                    caps_width(&row.chord)
                };
                let rect = Rect::new(right - width - 8, cap_y - 2, width + 8, KEYCAP + 4);
                self.push(
                    Kind::Keys,
                    rect,
                    Action::Capture(target.clone()),
                    &row.chord,
                    capturing,
                );
                right = rect.x;
            }
            None => {
                let total = caps_width(&row.chord);
                let mut x = right - total;
                for (text, key) in caps_of(&row.chord) {
                    let w = cap_width(&text, key);
                    self.out.caps.push(Cap {
                        rect: Rect::new(x, cap_y, w, KEYCAP),
                        text,
                        key,
                    });
                    x += w + GAP;
                }
                right -= total;
            }
        }
        for (label, action) in row.buttons.into_iter().rev() {
            let w = chip_width(label);
            let rect = Rect::new(right - GAP - w, self.y + (height - CONTROL) / 2, w, CONTROL);
            self.push(Kind::Button, rect, action, label, false);
            right = rect.x;
        }
        let text_w = (right - 12 - inner_x).max(40);
        let name_h = if row.note.is_some() { ROW - 12 } else { ROW };
        self.out.texts.push(Text {
            rect: Rect::new(inner_x, self.y, text_w, name_h),
            text: row.description.to_string(),
            tone: Tone::Name,
        });
        if let Some((note, tone)) = row.note {
            self.out.texts.push(Text {
                rect: Rect::new(inner_x, self.y + ROW - 20, text_w, 22),
                text: note,
                tone,
            });
        }
        self.y += height;
    }
}

/// A shortcut row to lay out.
struct ShortcutRow<'a> {
    /// What the shortcut does.
    description: &'a str,
    /// The chord's label, alternatives separated by ` / `; empty for a bind
    /// turned off.
    chord: String,
    /// What a click on the keys captures a chord for, when they can be
    /// changed.
    capture: Option<CaptureTarget>,
    /// Whether the keys are being captured now.
    capturing: bool,
    /// The buttons before the keys.
    buttons: Vec<(&'static str, Action)>,
    /// The line under the description, and how it is set.
    note: Option<(String, Tone)>,
}

impl<'a> ShortcutRow<'a> {
    /// A row that only shows its chord.
    fn fixed(description: &'a str, chord: &'a str) -> ShortcutRow<'a> {
        ShortcutRow {
            description,
            chord: chord.to_string(),
            capture: None,
            capturing: false,
            buttons: Vec::new(),
            note: None,
        }
    }
}

/// The caps of a chord's label: each key, and a slash between alternatives.
fn caps_of(chord: &str) -> Vec<(String, bool)> {
    let mut caps = Vec::new();
    for (i, alternative) in chord.split(" / ").enumerate() {
        if i > 0 {
            caps.push(("/".to_string(), false));
        }
        caps.extend(
            keybinds::label_keys(alternative)
                .into_iter()
                .map(|key| (key, true)),
        );
    }
    caps
}

/// Width of one cap, or of the slash between alternatives.
fn cap_width(text: &str, key: bool) -> i32 {
    if key { chip_width(text) - 10 } else { KEYCAP_SLASH }
}

/// Width the caps of a chord's label take with the gaps between them.
fn caps_width(chord: &str) -> i32 {
    let caps = caps_of(chord);
    let widths: i32 = caps.iter().map(|(text, key)| cap_width(text, *key)).sum();
    widths + GAP * (caps.len() as i32 - 1).max(0)
}

/// Whether a chord is being captured for a target.
fn capturing(d: &Desktop, target: &CaptureTarget) -> bool {
    d.settings.capture.as_ref().is_some_and(|c| &c.target == target)
}

/// The rows of the window manager's shortcuts: every bind of its
/// configuration, then the user's command binds.  A changed bind offers a
/// reset and a command bind its removal; a row whose chord another holds
/// names that one, and the row being captured says how to cancel.
///
/// # Arguments
/// * `published` - The shortcuts the window manager publishes.
/// * `capture` - The capture running, if any.
fn shortcut_rows<'a>(published: &'a Published, capture: Option<&Capture>) -> Vec<ShortcutRow<'a>> {
    let note = |spec: &str, own: &str, capturing: bool| {
        if capturing {
            return Some((
                tr("Escape cancels, Backspace turns the shortcut off").to_string(),
                Tone::Hint,
            ));
        }
        let others = published.others_on(spec, own);
        (!others.is_empty()).then(|| {
            (
                trf("Also used by {name}", &[("name", &others.join(", "))]),
                Tone::Warning,
            )
        })
    };
    let is_capturing = |target: &CaptureTarget| capture.is_some_and(|c| &c.target == target);
    let mut rows = Vec::new();
    for bind in &published.binds {
        let target = CaptureTarget::Rebind(bind.origin.clone());
        let capturing = is_capturing(&target);
        let mut buttons = Vec::new();
        if bind.changed() {
            buttons.push((tr("Reset"), Action::ResetBind(bind.origin.clone())));
        }
        rows.push(ShortcutRow {
            description: &bind.description,
            chord: chord_label(&bind.spec, &bind.label),
            note: note(&bind.spec, &bind.description, capturing),
            capture: Some(target),
            capturing,
            buttons,
        });
    }
    for custom in &published.custom {
        let target = CaptureTarget::Custom {
            spec: custom.spec.clone(),
            command: custom.command.clone(),
        };
        let capturing = is_capturing(&target);
        let remove = Action::RemoveCustom {
            spec: custom.spec.clone(),
            command: custom.command.clone(),
        };
        rows.push(ShortcutRow {
            description: &custom.command,
            chord: chord_label(&custom.spec, &custom.label),
            note: note(&custom.spec, &custom.command, capturing),
            capture: Some(target),
            capturing,
            buttons: vec![(tr("Remove"), remove)],
        });
    }
    rows
}

/// The label a shortcut row shows for a chord: the one the window manager
/// published, else one made from the chord; empty for a bind turned off.
fn chord_label(spec: &str, published: &str) -> String {
    match (spec.is_empty(), published.is_empty()) {
        (true, _) => String::new(),
        (false, true) => keybinds::label(spec),
        (false, false) => published.to_string(),
    }
}

/// What the page area shows at the moment.
fn view(d: &Desktop) -> View {
    d.settings.picking.map_or(View::Page(d.settings.page), View::List)
}

/// Lays the window out as it is.
fn layout(d: &Desktop) -> Layout {
    build(d, frame(d), d.settings.scroll, view(d))
}

/// The scrollbar of the page area, when the page or the application list
/// is taller than it.
fn scrollbar(d: &Desktop) -> Option<ui::Scrollbar> {
    let frame = frame(d);
    let content = build(d, frame, 0.0, view(d)).content;
    let page = scroll_rect_in(frame, view(d));
    let gutter = Rect::new(page.x, page.y, page.w + ui::SCROLLBAR, page.h);
    ui::Scrollbar::new(gutter, content, d.settings.scroll)
}

/// Lays the sidebar out: the pages, each group under its caption.
fn build_sidebar(d: &Desktop, frame: Rect, out: &mut Layout) {
    let fixed = |rect: Rect, action: Action, label: &str, active: bool, kind: Kind| Control {
        rect,
        action,
        label: label.to_string(),
        active,
        kind,
        color: None,
        app: None,
        look: None,
        wallpaper: None,
        clip: None,
    };
    let column = sidebar_rect_in(frame);
    let mut y = column.y;
    let mut first = true;
    for group in Group::ALL {
        let members: Vec<Page> = Page::ALL.into_iter().filter(|p| p.group() == group).collect();
        if members.is_empty() {
            continue;
        }
        if !first {
            out.nav_lines
                .push(Rect::new(column.x + 6, y + NAV_GROUP_GAP / 2, column.w - 12, 1));
            y += NAV_GROUP_GAP;
        }
        first = false;
        out.nav_texts.push(Text {
            rect: Rect::new(column.x + 12, y, column.w - 24, NAV_CAPTION),
            text: group.name().to_uppercase(),
            tone: Tone::Caption,
        });
        y += NAV_CAPTION;
        for page in members {
            out.controls.push(fixed(
                Rect::new(column.x, y, column.w, NAV_ROW),
                Action::Page(page),
                page.name(),
                d.settings.page == page,
                Kind::Nav,
            ));
            y += NAV_ROW + 2;
        }
    }
}

/// Lays the window out inside a frame.
///
/// # Arguments
/// * `d` - The desktop.
/// * `frame` - The window rectangle.
/// * `scroll` - How far the page is scrolled.
/// * `view` - The page or the application list laid out.
fn build(d: &Desktop, frame: Rect, scroll: f64, view: View) -> Layout {
    let page = scroll_rect_in(frame, view);
    let mut out = Layout::default();
    let pinned = pinned_height(view);
    if pinned > 0 {
        let area = page_rect_in(frame);
        out.pinned = Some(Rect::new(area.x, area.y, area.w, pinned - GROUP_GAP));
    }
    build_sidebar(d, frame, &mut out);
    let footer = |label: &str, action: Action| Control {
        rect: Rect::new(
            frame.right() - PAD - chip_width(label),
            frame.bottom() - PAD - CONTROL,
            chip_width(label),
            CONTROL,
        ),
        action,
        label: label.to_string(),
        active: false,
        kind: Kind::Button,
        color: None,
        app: None,
        look: None,
        wallpaper: None,
        clip: None,
    };

    if let View::List(key) = view {
        out.controls.push(footer(tr("Back"), Action::Back));
        let (title, members) = if key == "favorites" {
            (tr("Favorites"), &d.config.favorites)
        } else {
            (tr("Quick launch"), &d.config.launchers)
        };
        out.texts.push(Text {
            rect: Rect::new(page.x + CARD_PAD, page.y, page.w - 2 * CARD_PAD, CAPTION),
            text: title.to_string(),
            tone: Tone::Caption,
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
                look: None,
                wallpaper: None,
                clip: Some(list),
            });
        }
        out.content = d.apps.entries.len() as i32 * LIST_ROW + CAPTION;
        return out;
    }

    if !page_keys(d).is_empty() {
        out.controls.push(footer(tr("Reset"), Action::Reset));
    }
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
            groups.switch(tr("Flat icon"), "start_icon_flat", d.config.start_icon_flat);
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
            groups.switch(tr("Float"), "panel_float", d.config.panel_float);
            if d.config.panel_float {
                groups.slider(
                    tr("Margin"),
                    "panel_margin",
                    slider_value(d, "panel_margin"),
                    "px",
                );
                groups.slider(
                    tr("Corners"),
                    "panel_corners",
                    slider_value(d, "panel_corners"),
                    "px",
                );
            }
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
        Page::Desktops => {
            groups.group(tr("Virtual desktops"));
            groups.slider(tr("Desktops"), "desktops", slider_value(d, "desktops"), "");
            groups.switch(tr("Show the pager"), "show_pager", d.config.show_pager);
            groups.switch(
                tr("Show windows of all desktops"),
                "tasks_all_desktops",
                d.config.tasks_all_desktops,
            );
        }
        Page::Shortcuts => {
            groups.note(tr(
                "Combinations your browser or operating system take for themselves never reach the desktop; pick another combination for those.",
            ));
            groups.group(tr("Window manager"));
            match &d.settings.keybinds {
                Some(published) => {
                    if published.binds.is_empty() && published.custom.is_empty() {
                        groups.info(tr("No shortcuts published"), "");
                    }
                    for row in shortcut_rows(published, d.settings.capture.as_ref()) {
                        groups.shortcut(row);
                    }
                    let adding = capturing(d, &CaptureTarget::Add);
                    let label = if adding {
                        tr("Press a key combination…")
                    } else {
                        tr("Choose keys…")
                    };
                    groups.field(
                        tr("Add shortcut"),
                        Field::Command,
                        field_text(d, Field::Command),
                        focus == Some(Field::Command),
                        &[(label, Action::Capture(CaptureTarget::Add))],
                    );
                    if let Some(button) = groups.out.controls.last_mut() {
                        button.active = adding;
                    }
                }
                None => groups.info(tr("No shortcuts published"), ""),
            }
            groups.group(tr("Desktop"));
            for (chord, description) in desktop_shortcuts() {
                groups.shortcut(ShortcutRow::fixed(description, chord));
            }
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
        Page::Language => {
            groups.group(tr("Language"));
            let mut options: Vec<(String, Action)> = vec![(tr("System").to_string(), set("language", ""))];
            options.extend(
                LANGUAGES
                    .iter()
                    .map(|(code, name)| (name.to_string(), set("language", code))),
            );
            let current = d.config.language.as_deref().map(|code| {
                let code = i18n::normalize(code).unwrap_or("en");
                LANGUAGES
                    .iter()
                    .find(|(c, _)| *c == code)
                    .map_or("English", |(_, name)| *name)
            });
            groups.segments(tr("Language"), &options, current.unwrap_or(tr("System")));
        }
        Page::About => {
            groups.group(tr("About"));
            groups.info(tr("Version"), env!("CARGO_PKG_VERSION"));
            groups.info(tr("Backend"), &d.backend);
            groups.info(tr("Desktops"), &d.desktops.to_string());
            groups.info(tr("Config file"), &config::config_file().display().to_string());
        }
        Page::Wallpaper => {
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
            let image_key = wallpaper_key(d, "wallpaper");
            let mode_key = wallpaper_key(d, "wallpaper_mode");
            let shown = wallpaper.image.as_deref().map(crate::wallpapers::real_path);
            let choose = |path: &str, mode: &str| {
                Action::Set(vec![(image_key, path.to_string()), (mode_key, mode.to_string())])
            };
            let mut cards = Vec::new();
            let logo = Path::new(config::LOGO_PATH);
            if logo.is_file() {
                cards.push(WallpaperChoice {
                    name: tr("Application icon").to_string(),
                    card: WallpaperCard {
                        light: None,
                        dark: None,
                        author: None,
                    },
                    action: choose(config::LOGO_PATH, "center"),
                    active: shown.as_deref() == Some(crate::wallpapers::real_path(logo).as_path()),
                });
            }
            for entry in &d.settings.wallpapers {
                cards.push(WallpaperChoice {
                    name: entry.name.clone(),
                    card: WallpaperCard {
                        light: entry.light.clone(),
                        dark: entry.dark.clone(),
                        author: entry.author.clone(),
                    },
                    action: choose(&entry.path.to_string_lossy(), "fill"),
                    active: shown.as_ref() == Some(&entry.real),
                });
            }
            groups.wallpapers(cards);
            if d.settings.monitor.is_none() {
                groups.segments(
                    tr("Light or dark"),
                    &[
                        (tr("Automatic").to_string(), set("wallpaper_variant", "automatic")),
                        (tr("Light").to_string(), set("wallpaper_variant", "light")),
                        (tr("Dark").to_string(), set("wallpaper_variant", "dark")),
                    ],
                    match d.config.wallpaper_variant {
                        config::WallpaperVariant::Automatic => tr("Automatic"),
                        config::WallpaperVariant::Light => tr("Light"),
                        config::WallpaperVariant::Dark => tr("Dark"),
                    },
                );
            }
            groups.field(
                tr("Image"),
                Field::Image,
                field_text(d, Field::Image),
                focus == Some(Field::Image),
                &[
                    (tr("Browse…"), Action::Browse),
                    (tr("Clear"), set(image_key, "none")),
                ],
            );
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
            // An image filling the screen leaves nothing of the background to see.
            let covered = wallpaper.image.is_some()
                && matches!(wallpaper.mode, WallpaperMode::Fill | WallpaperMode::Stretch);
            if !covered {
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
            }
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
            let scheme = current_scheme(d.config.theme);
            groups.group(tr("Scheme"));
            let schemes: Vec<PresetCard> = SCHEMES
                .iter()
                .map(|(name, colors)| {
                    let changes = COLOR_KEYS
                        .iter()
                        .zip(colors)
                        .map(|(key, color)| (*key, color.to_string()))
                        .collect();
                    let mut look = preview::Look::current(d);
                    look.theme = scheme_theme(colors);
                    PresetCard {
                        name: preset_name(name).to_string(),
                        look,
                        action: Action::Set(changes),
                        active: scheme == Some(*name),
                    }
                })
                .collect();
            groups.presets(schemes);
            let style = current_style(&d.config);
            groups.group(tr("Style"));
            let styles: Vec<PresetCard> = STYLES
                .iter()
                .map(|s| PresetCard {
                    name: preset_name(s.name).to_string(),
                    look: s.look(d),
                    action: Action::Set(s.changes()),
                    active: style == Some(s.name),
                })
                .collect();
            groups.presets(styles);
        }
        Page::Colors => {
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
        }
        Page::Windows => {
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
        look: None,
        wallpaper: None,
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
        "panel_corners" => d.config.panel_corners,
        "window_corners" => d.config.window.corners,
        "desktops" => d.config.desktops as i32,
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

/// Draws a flat button: a tonal pill with its label, ringed with the accent
/// while what it started runs.
fn draw_button(d: &mut Desktop, canvas: &mut Canvas<'_>, control: &Control, hovered: bool) {
    let rect = control.rect;
    canvas.fill_rounded(rect, 7.0, ui::text_alpha(if hovered { 0x2a } else { 0x16 }));
    if control.active {
        canvas.stroke_rounded(rect, 7.0, ACCENT_STROKE, ui::accent());
    }
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

/// Draws a text field with its caret when focused.  An empty field shows
/// what it is for: the command of a shortcut
/// being added that it takes one, the others `none`.
fn draw_field(d: &mut Desktop, canvas: &mut Canvas<'_>, control: &Control, hovered: bool) {
    let rect = control.rect;
    canvas.fill_rounded(rect, 7.0, ui::text_alpha(0x10));
    if control.active {
        canvas.stroke_rounded(rect, 7.0, ACCENT_STROKE, ui::accent());
    } else if hovered {
        canvas.stroke_rounded(rect, 7.0, 1.0, ui::text_dim());
    } else {
        canvas.stroke_rounded(rect, 7.0, 1.0, ui::popup_border());
    }
    let text_rect = Rect::new(rect.x + 8, rect.y, rect.w - 16, rect.h);
    let empty = control.label.is_empty();
    let shown = if empty && !control.active {
        match control.action {
            Action::Focus(Field::Command) => tr("Command").to_string(),
            _ => "none".to_string(),
        }
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
        let scale = d.settings.geometry.scale;
        let caret_x = text_rect.x
            + ui::text_width(&mut d.fonts, &control.label, ui::FONT_SIZE, false, scale).min(text_rect.w - 2);
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
    draw_pill_switch(canvas, pill, control.active);
}

/// Draws a toggle switch: an accent pill with its knob at the right when on,
/// a neutral one with its knob at the left when off.
///
/// # Arguments
/// * `canvas` - The target.
/// * `pill` - The switch, [`SWITCH_WIDTH`] by [`SWITCH_HEIGHT`].
/// * `on` - Whether it is on.
pub fn draw_pill_switch(canvas: &mut Canvas<'_>, pill: Rect, on: bool) {
    let radius = pill.h as f32 / 2.0;
    let track = if on { ui::accent() } else { ui::text_alpha(0x30) };
    canvas.fill_rounded(pill, radius, track);
    let knob = pill.h - 6;
    let knob_x = if on { pill.right() - 3 - knob } else { pill.x + 3 };
    canvas.fill_rounded(
        Rect::new(knob_x, pill.y + 3, knob, knob),
        knob as f32 / 2.0,
        if on { ui::on_accent() } else { ui::text() },
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

/// The rendered preview of a look at a scale, shrunk to a width in
/// pixels, from the cache or drawn now.  The shrink filters the whole
/// image, which drawing it scaled down would only sample.
fn preset_image(d: &mut Desktop, look: &preview::Look, scale: i32, width: u32) -> Option<Arc<Pixmap>> {
    let wallpaper = d.config.wallpaper_for(1);
    let key = format!(
        "{}{}{}{}:{:?}:{}:{}:{:?}:{:?}:{:?}:{:?}:{}:{scale}:{width}",
        hex(look.theme.accent),
        hex(look.theme.panel),
        hex(look.theme.popup),
        hex(look.theme.text),
        look.style,
        look.centered,
        look.margin,
        look.menu,
        wallpaper.image,
        wallpaper.mode,
        d.config.wallpaper_variant,
        if wallpaper.color_set {
            hex(wallpaper.color)
        } else {
            String::new()
        },
    );
    if let Some(image) = d.settings.presets.get(&key) {
        return Some(image.clone());
    }
    let full = preview::render(d, look, scale)?;
    let image = Arc::new(crate::render::resize_pixmap(&full, width.clamp(1, full.width()))?);
    if d.settings.presets.len() >= 32 {
        d.settings.presets.clear();
    }
    d.settings.presets.insert(key, image.clone());
    Some(image)
}

/// Draws a preset card: the look's preview shrunk into the card over its
/// name, ringed with the accent when it is in effect.
fn draw_preset(d: &mut Desktop, canvas: &mut Canvas<'_>, control: &Control, hovered: bool) {
    let rect = control.rect;
    canvas.fill_rounded(
        rect,
        CARD_RADIUS,
        ui::text_alpha(if hovered { 0x1c } else { 0x0c }),
    );
    let image_rect = Rect::new(
        rect.x + PRESET_PAD,
        rect.y + PRESET_PAD,
        rect.w - 2 * PRESET_PAD,
        rect.h - 2 * PRESET_PAD - PRESET_NAME,
    );
    let scale = canvas.scale();
    let width = (image_rect.w * scale).max(1) as u32;
    if let Some(look) = &control.look
        && let Some(image) = preset_image(d, look, scale, width)
    {
        canvas.clip(image_rect);
        canvas.draw_image_fit(&image, image_rect);
        canvas.reset_clip();
        if let Some(clip) = control.clip {
            canvas.clip(clip);
        }
    }
    canvas.stroke_rounded(image_rect, 3.0, 1.0, ui::popup_border());
    ui::label(
        canvas,
        &mut d.fonts,
        &control.label,
        Rect::new(
            rect.x + PRESET_PAD,
            image_rect.bottom(),
            rect.w - 2 * PRESET_PAD,
            PRESET_NAME,
        ),
        if rect.w < PRESET_SMALL_NAME {
            ui::FONT_SMALL
        } else {
            ui::FONT_SIZE
        },
        control.active,
        Align::Center,
        ui::text(),
    );
    if control.active {
        canvas.stroke_rounded(rect, CARD_RADIUS, ACCENT_STROKE, ui::accent());
    } else if hovered {
        canvas.stroke_rounded(rect, CARD_RADIUS, 1.0, ui::text_dim());
    }
}

/// Draws a wallpaper card: its thumbnail, the light picture on the left
/// and the dark one on the right where it has both, the application icon
/// on the color it would be painted on for the icon's card, and the name and
/// author under it, the one shown ringed with the accent.
fn draw_wallpaper_card(d: &mut Desktop, canvas: &mut Canvas<'_>, control: &Control, hovered: bool) {
    let rect = control.rect;
    canvas.fill_rounded(
        rect,
        CARD_RADIUS,
        ui::text_alpha(if hovered { 0x1c } else { 0x0c }),
    );
    let image_rect = Rect::new(
        rect.x + PRESET_PAD,
        rect.y + PRESET_PAD,
        rect.w - 2 * PRESET_PAD,
        rect.h - 2 * PRESET_PAD - PRESET_NAME - WALLPAPER_AUTHOR,
    );
    let Some(card) = control.wallpaper.clone() else {
        return;
    };
    let size = (image_rect.w * canvas.scale()).max(1) as u32;
    match &card.light {
        None => {
            let color = d.painted_wallpaper(config::Wallpaper::default(), (1, 1)).color;
            canvas.fill_rect(image_rect, color);
            let side = image_rect.h * 3 / 5;
            if let Some(logo) = d.logo.as_ref() {
                canvas.draw_image_fit(
                    logo,
                    Rect::new(
                        image_rect.x + (image_rect.w - side) / 2,
                        image_rect.y + (image_rect.h - side) / 2,
                        side,
                        side,
                    ),
                );
            }
        }
        Some(light) => {
            let half = image_rect.w / 2;
            let (light_part, dark_part) = match card.dark {
                Some(_) => (
                    Rect::new(image_rect.x, image_rect.y, half, image_rect.h),
                    Rect::new(
                        image_rect.x + half,
                        image_rect.y,
                        image_rect.w - half,
                        image_rect.h,
                    ),
                ),
                None => (image_rect, Rect::new(0, 0, 0, 0)),
            };
            draw_thumbnail(d, canvas, light, size, image_rect, light_part);
            if let Some(dark) = &card.dark {
                draw_thumbnail(d, canvas, dark, size, image_rect, dark_part);
            }
        }
    }
    if let Some(clip) = control.clip {
        canvas.clip(clip);
    }
    canvas.stroke_rounded(image_rect, 3.0, 1.0, ui::popup_border());
    let name_rect = Rect::new(
        rect.x + PRESET_PAD,
        image_rect.bottom() + 2,
        rect.w - 2 * PRESET_PAD,
        PRESET_NAME,
    );
    ui::label(
        canvas,
        &mut d.fonts,
        &control.label,
        name_rect,
        ui::FONT_SIZE,
        control.active,
        Align::Center,
        ui::text(),
    );
    if let Some(author) = &card.author {
        ui::label(
            canvas,
            &mut d.fonts,
            author,
            Rect::new(name_rect.x, name_rect.bottom() - 4, name_rect.w, WALLPAPER_AUTHOR),
            ui::FONT_SMALL,
            false,
            Align::Center,
            ui::text_dim(),
        );
    }
    if control.active {
        canvas.stroke_rounded(rect, CARD_RADIUS, ACCENT_STROKE, ui::accent());
    } else if hovered {
        canvas.stroke_rounded(rect, CARD_RADIUS, 1.0, ui::text_dim());
    }
}

/// Draws a picture's thumbnail covering a card's thumbnail area, cut to one
/// part of it, a placeholder until the thumbnail is decoded.
///
/// # Arguments
/// * `path` - The picture.
/// * `size` - The thumbnail's longer side in physical pixels.
/// * `frame` - The card's thumbnail area, which the picture covers.
/// * `part` - The part of it drawn.
fn draw_thumbnail(d: &mut Desktop, canvas: &mut Canvas<'_>, path: &Path, size: u32, frame: Rect, part: Rect) {
    canvas.clip(part);
    match d.files.image_thumbnail(path, size) {
        Some(image) => {
            let scale = canvas.scale().max(1) as f32;
            let native = (image.width() as f32 / scale, image.height() as f32 / scale);
            let (w, h) = crate::ui::background::drawn_size(WallpaperMode::Fill, native, frame);
            let (w, h) = (w.round() as i32, h.round() as i32);
            canvas.draw_image_stretch(
                &image,
                Rect::new(frame.x + (frame.w - w) / 2, frame.y + (frame.h - h) / 2, w, h),
            );
        }
        None => canvas.fill_rect(part, ui::text_alpha(0x10)),
    }
    canvas.reset_clip();
}

/// Draws a shortcut's chord that a click captures anew: its caps flush
/// right on a hover highlight, `Off` for a bind turned off, and while it
/// captures the prompt in a field ringed with the accent.
fn draw_keys(d: &mut Desktop, canvas: &mut Canvas<'_>, control: &Control, hovered: bool) {
    let rect = control.rect;
    if control.active {
        canvas.fill_rounded(rect, 7.0, ui::text_alpha(0x10));
        canvas.stroke_rounded(rect, 7.0, ACCENT_STROKE, ui::accent());
        ui::label(
            canvas,
            &mut d.fonts,
            tr("Press a key combination…"),
            Rect::new(rect.x + 6, rect.y, rect.w - 12, rect.h),
            ui::FONT_SMALL,
            false,
            Align::Center,
            ui::text(),
        );
        return;
    }
    if hovered {
        canvas.fill_rounded(rect, 7.0, ui::hover_bg());
    }
    let inner = Rect::new(rect.x + 4, rect.y + 2, rect.w - 8, KEYCAP);
    if control.label.is_empty() {
        canvas.stroke_rounded(inner, 5.0, 1.0, ui::popup_border());
        ui::label(
            canvas,
            &mut d.fonts,
            tr("Off"),
            inner,
            ui::FONT_SMALL,
            false,
            Align::Center,
            ui::text_dim(),
        );
        return;
    }
    let mut x = inner.right() - caps_width(&control.label);
    for (text, key) in caps_of(&control.label) {
        let w = cap_width(&text, key);
        let cap = Cap {
            rect: Rect::new(x, inner.y, w, KEYCAP),
            text,
            key,
        };
        draw_cap(d, canvas, &cap);
        x += w + GAP;
    }
}

/// Draws a key cap, or the slash between alternative chords.
fn draw_cap(d: &mut Desktop, canvas: &mut Canvas<'_>, cap: &Cap) {
    if cap.key {
        canvas.fill_rounded(cap.rect, 5.0, ui::text_alpha(0x1c));
        canvas.stroke_rounded(cap.rect, 5.0, 1.0, ui::popup_border());
    }
    ui::label(
        canvas,
        &mut d.fonts,
        &cap.text,
        cap.rect,
        ui::FONT_SMALL,
        false,
        Align::Center,
        if cap.key { ui::text() } else { ui::text_dim() },
    );
}

/// Draws one control.
fn draw_control(d: &mut Desktop, canvas: &mut Canvas<'_>, control: &Control, hovered: bool) {
    let rect = control.rect;
    match control.kind {
        Kind::Nav => {
            if control.active {
                canvas.fill_rounded(rect, 7.0, ui::accent());
            } else if hovered {
                canvas.fill_rounded(rect, 7.0, ui::hover_bg());
            }
            ui::label(
                canvas,
                &mut d.fonts,
                &control.label,
                Rect::new(rect.x + 12, rect.y, rect.w - 22, rect.h),
                ui::FONT_SIZE,
                control.active,
                Align::Left,
                if control.active {
                    ui::on_accent()
                } else {
                    ui::text()
                },
            );
        }
        Kind::Keys => draw_keys(d, canvas, control, hovered),
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
                if let Some(path) = ui::cross_path(rect, 5.0) {
                    canvas.stroke_path(&path, 2.0, ui::popup_bg());
                }
            }
        }
        Kind::Text => draw_field(d, canvas, control, hovered),
        Kind::Swatch => {
            if let Some(color) = control.color {
                canvas.fill_rounded(rect, 5.0, color);
            }
            if control.active {
                canvas.stroke_rounded(rect, 5.0, ACCENT_STROKE, ui::accent());
            } else {
                canvas.stroke_rounded(
                    rect,
                    5.0,
                    1.0,
                    if hovered { ui::text() } else { ui::popup_border() },
                );
            }
        }
        Kind::Switch => draw_switch(canvas, control, hovered),
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
            if control.active {
                canvas.stroke_rounded(rect, 5.0, ACCENT_STROKE, ui::accent());
            } else {
                canvas.stroke_rounded(
                    rect,
                    5.0,
                    1.0,
                    if hovered { ui::text() } else { ui::popup_border() },
                );
            }
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
        Kind::Preset => draw_preset(d, canvas, control, hovered),
        Kind::Wallpaper => draw_wallpaper_card(d, canvas, control, hovered),
    }
}

/// Draws the window: the sidebar, the page in its cards, the scrollbar and
/// the open color picker over everything.
///
/// # Arguments
/// * `d` - The desktop.
/// * `canvas` - The window's canvas.
pub fn draw(d: &mut Desktop, canvas: &mut Canvas<'_>) {
    sync_picker(d);
    let frame = frame(d);
    let page = scroll_rect_in(frame, view(d));
    canvas.fill_rect(frame, ui::popup_bg());
    canvas.fill_rect(
        Rect::new(frame.x, frame.y, SIDEBAR, frame.h),
        ui::text_alpha(0x0a),
    );
    canvas.fill_rect(Rect::new(frame.x + SIDEBAR, frame.y, 1, frame.h), ui::separator());
    let layout = layout(d);
    let first = layout.picker.map_or(layout.controls.len(), |picker| picker.first);
    for (index, control) in layout.controls.iter().enumerate().take(first) {
        if control.clip.is_none() {
            draw_control(d, canvas, control, d.hover == Hover::Control(index));
        }
    }
    for line in &layout.nav_lines {
        canvas.fill_rect(*line, ui::separator());
    }
    for text in &layout.nav_texts {
        ui::label(
            canvas,
            &mut d.fonts,
            &text.text,
            text.rect,
            NAV_CAPTION_FONT,
            true,
            Align::Left,
            ui::text_alpha(0x8c),
        );
    }
    if let Some(card) = layout.pinned {
        draw_pinned(d, canvas, card);
    }
    canvas.clip(page);
    for card in &layout.cards {
        canvas.fill_rounded(*card, CARD_RADIUS, ui::field_bg());
    }
    for line in &layout.lines {
        canvas.fill_rect(*line, ui::separator());
    }
    for text in &layout.texts {
        draw_text(d, canvas, text);
    }
    for cap in &layout.caps {
        draw_cap(d, canvas, cap);
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
}

/// Draws the preview a page pins above its rows in its card: the live
/// look on the Theme page, the monitor shown on the Wallpaper page.
fn draw_pinned(d: &mut Desktop, canvas: &mut Canvas<'_>, card: Rect) {
    canvas.fill_rounded(card, CARD_RADIUS, ui::field_bg());
    let area = Rect::new(
        card.x + CARD_PAD,
        card.y + PREVIEW_PAD,
        card.w - 2 * CARD_PAD,
        card.h - 2 * PREVIEW_PAD,
    );
    canvas.clip(card);
    match d.settings.page {
        Page::Wallpaper => {
            let wallpaper = shown_wallpaper(d);
            let monitor = d.settings.monitor;
            preview::draw_monitor(d, canvas, area, &wallpaper, monitor);
        }
        _ => preview::draw(d, canvas, area, card),
    }
    canvas.reset_clip();
}

/// Draws a piece of the page's text as its tone sets it.
fn draw_text(d: &mut Desktop, canvas: &mut Canvas<'_>, text: &Text) {
    let (size, bold, align, color) = match text.tone {
        Tone::Name => (ui::FONT_SIZE, false, Align::Left, ui::text()),
        Tone::Caption => (ui::FONT_SIZE, true, Align::Left, ui::text_dim()),
        Tone::Value => (ui::FONT_SIZE, false, Align::Right, ui::text_dim()),
        Tone::Hint => (ui::FONT_SMALL, false, Align::Left, ui::text_dim()),
        Tone::Warning => {
            let mark = Rect::new(
                text.rect.x,
                text.rect.y + (text.rect.h - WARNING_MARK) / 2,
                WARNING_MARK,
                WARNING_MARK,
            );
            canvas.fill_rounded(mark, 2.0, warning());
            let rest = Rect::new(
                mark.right() + 6,
                text.rect.y,
                text.rect.w - WARNING_MARK - 6,
                text.rect.h,
            );
            ui::label(
                canvas,
                &mut d.fonts,
                &text.text,
                rest,
                ui::FONT_SMALL,
                false,
                Align::Left,
                ui::text(),
            );
            return;
        }
        Tone::Note => {
            let scale = canvas.scale() as f32;
            let (first, second) = ui::wrap_two_lines(
                &mut d.fonts,
                &text.text,
                ui::FONT_SMALL * scale,
                text.rect.w as f32 * scale,
            );
            let rect = text.rect;
            let color = ui::text_dim();
            canvas.draw_text(
                &mut d.fonts,
                &first,
                rect.x,
                rect.y + NOTE_LINE / 2,
                rect.w,
                Align::Left,
                color,
            );
            if let Some(second) = second {
                canvas.draw_text(
                    &mut d.fonts,
                    &second,
                    rect.x,
                    rect.y + NOTE_LINE + NOTE_LINE / 2,
                    rect.w,
                    Align::Left,
                    color,
                );
            }
            return;
        }
    };
    ui::label(
        canvas,
        &mut d.fonts,
        &text.text,
        text.rect,
        size,
        bold,
        align,
        color,
    );
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

/// Follows the pointer in the window: a drag in progress, else the hover.
///
/// # Arguments
/// * `d` - The desktop.
/// * `x` - Pointer X.
/// * `y` - Pointer Y.
pub fn pointer_motion(d: &mut Desktop, x: f64, y: f64) {
    d.settings.pointer = (x, y);
    if d.settings.dragging() {
        drag(d, x, y);
        return;
    }
    let hover = hit(d, x, y).unwrap_or(Hover::None);
    d.set_hover(hover);
}

/// Clears the hover when the pointer leaves the window.
pub fn pointer_leave(d: &mut Desktop) {
    d.settings.pointer = (-1.0, -1.0);
    if matches!(d.hover, Hover::Control(_)) {
        d.set_hover(Hover::None);
    }
}

/// Routes a button press to the window, or ends a drag on the release.
///
/// # Arguments
/// * `d` - The desktop.
/// * `x` - Pointer X.
/// * `y` - Pointer Y.
/// * `button` - The button.
/// * `pressed` - Whether the button went down.
pub fn pointer_button(d: &mut Desktop, x: f64, y: f64, button: Button, pressed: bool) {
    d.settings.pointer = (x, y);
    if pressed {
        click(d, x, y, button);
    } else if d.settings.dragging() {
        release(d);
    }
}

/// Handles a click.
///
/// # Arguments
/// * `d` - The desktop.
/// * `x` - Pointer X.
/// * `y` - Pointer Y.
/// * `button` - The button.
fn click(d: &mut Desktop, x: f64, y: f64, button: Button) {
    let Some(hover) = hit(d, x, y) else {
        return;
    };
    if button != Button::Left {
        return;
    }
    let layout = layout(d);
    // A click anywhere but in the open picker closes it, then goes on to
    // whatever it landed on.
    let open = d.settings.picker.map(|picker| picker.field);
    if open.is_some() && !layout.picker.is_some_and(|picker| picker.frame.contains(x, y)) {
        d.settings.picker = None;
    }
    // A click ends a capture, and one on the keys being captured does no more.
    let captured = d.settings.capture.as_ref().map(|c| c.target.clone());
    if captured.is_some() {
        cancel_capture(d);
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
        d.redraw_settings();
        return;
    };
    let Some(control) = layout.controls.get(index) else {
        return;
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
            return;
        }
        Action::BrowseIcon => {
            d.pick_start_icon();
            return;
        }
        Action::Page(page) => show_page(d, page),
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
        Action::OpenSettings(_)
        | Action::EditFavorites
        | Action::Reload
        | Action::ToggleMute
        | Action::RemoveLauncher(_)
        | Action::ToggleDnd
        | Action::ClearNotifications => {}
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
        Action::Capture(target) => {
            if captured.as_ref() != Some(&target) {
                start_capture(d, target);
            }
        }
        Action::ResetBind(origin) => {
            edit_keybinds(d, |text| keybinds::with_rebind(text, &origin, None));
        }
        Action::RemoveCustom { spec, command } => {
            edit_keybinds(d, |text| keybinds::without_execute(text, &spec, &command));
        }
    }
    d.set_hover(hit(d, x, y).unwrap_or(Hover::None));
    d.redraw_settings();
}

/// Shows a page from the top, leaving the application list, and reads the
/// published shortcuts again for the Shortcuts page.
fn show_page(d: &mut Desktop, page: Page) {
    cancel_capture(d);
    d.settings.page = page;
    d.settings.picking = None;
    d.settings.scroll = 0.0;
    if page == Page::Shortcuts {
        d.settings.reread_keybinds();
        d.settings.poll_at = Some(Instant::now() + KEYBINDS_POLL);
    }
    if page == Page::Wallpaper {
        d.settings.wallpapers = crate::wallpapers::list();
    }
}

/// Starts capturing a chord for a shortcut, holding the keyboard for the
/// window meanwhile.  A shortcut being added needs its command first, so
/// without one the command's field takes the focus instead.
fn start_capture(d: &mut Desktop, target: CaptureTarget) {
    if target == CaptureTarget::Add && d.settings.command.trim().is_empty() {
        d.settings.focus = Some(Field::Command);
        d.settings.editing = d.settings.command.clone();
        return;
    }
    let Some(id) = d.settings.id else {
        return;
    };
    d.settings.capture = Some(Capture { target, held: None });
    d.requests.push(Request::GrabKeyboard { id, grab: true });
}

/// Ends a capture without a chord, letting the keyboard go.
pub fn cancel_capture(d: &mut Desktop) {
    finish_capture(d, None);
}

/// Ends a capture, writing the chord captured to the overrides: a bind
/// moved to it, a command bind moved to it, or the bind being added.  An
/// empty chord turns a bind off and removes a command bind.
///
/// # Arguments
/// * `d` - The desktop.
/// * `spec` - The chord, empty for none, or `None` when the capture was
///   canceled.
fn finish_capture(d: &mut Desktop, spec: Option<String>) {
    let Some(capture) = d.settings.capture.take() else {
        return;
    };
    if let Some(id) = d.settings.id {
        d.requests.push(Request::GrabKeyboard { id, grab: false });
    }
    d.redraw_settings();
    let Some(spec) = spec else {
        return;
    };
    match capture.target {
        CaptureTarget::Rebind(origin) => {
            edit_keybinds(d, |text| keybinds::with_rebind(text, &origin, Some(&spec)));
        }
        CaptureTarget::Custom { spec: old, command } => edit_keybinds(d, |text| {
            let text = keybinds::without_execute(text, &old, &command);
            if spec.is_empty() {
                text
            } else {
                keybinds::with_execute(&text, &spec, &command)
            }
        }),
        CaptureTarget::Add => {
            let command = d.settings.command.trim().to_string();
            if !spec.is_empty() && !command.is_empty() {
                d.settings.command.clear();
                edit_keybinds(d, |text| keybinds::with_execute(text, &spec, &command));
            }
        }
    }
}

/// Edits the shortcut overrides and reads the published shortcuts again,
/// which show the change once the window manager applied it.
fn edit_keybinds(d: &mut Desktop, edit: impl FnOnce(&str) -> String) {
    if let Err(error) = keybinds::edit_overrides(edit) {
        eprintln!(
            "selkies-desktop: cannot write {}: {error}",
            keybinds::overrides_path().display()
        );
    }
    d.settings.reread_keybinds();
    clamp_scroll(d);
    d.redraw_settings();
}

/// What a key going down or up does to a capture.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Step {
    /// Nothing is decided yet.
    Wait,
    /// The capture is canceled.
    Cancel,
    /// The chord is captured, empty to turn the shortcut off.
    Chord(String),
}

/// Turns a key going down or up into the next step of a capture: a key
/// with the modifiers held makes the chord, and so does a modifier alone
/// released before anything else was pressed.  Escape alone cancels, and
/// Backspace alone turns the shortcut off.
///
/// # Arguments
/// * `held` - The modifier pressed alone so far, kept between keys.
/// * `keysym` - The key's unshifted keysym.
/// * `modifiers` - The modifiers held as the key went down or up.
/// * `pressed` - Whether the key went down.
fn capture_step(held: &mut Option<u32>, keysym: u32, modifiers: Modifiers, pressed: bool) -> Step {
    let alone = modifiers == Modifiers::default();
    if keybinds::is_modifier(keysym) {
        if pressed {
            *held = (alone && held.is_none()).then_some(keysym);
        } else if *held == Some(keysym) {
            return Step::Chord(keybinds::key_name(keysym));
        } else {
            *held = None;
        }
        return Step::Wait;
    }
    if !pressed {
        return Step::Wait;
    }
    if alone && keysym == Keysym::Escape.raw() {
        return Step::Cancel;
    }
    if alone && keysym == Keysym::BackSpace.raw() {
        return Step::Chord(String::new());
    }
    Step::Chord(keybinds::spec(modifiers, keysym))
}

/// Feeds a key going down or up to the capture running, finishing it when
/// the key decides it.
///
/// # Arguments
/// * `d` - The desktop.
/// * `keysym` - The key's unshifted keysym.
/// * `modifiers` - The modifiers held as the key went down or up.
/// * `pressed` - Whether the key went down.
pub fn key_sym(d: &mut Desktop, keysym: u32, modifiers: Modifiers, pressed: bool) {
    let Some(capture) = d.settings.capture.as_mut() else {
        return;
    };
    match capture_step(&mut capture.held, keysym, modifiers, pressed) {
        Step::Wait => {}
        Step::Cancel => cancel_capture(d),
        Step::Chord(chord) => finish_capture(d, Some(chord)),
    }
}

/// Reads the published shortcuts every second while the Shortcuts page is
/// shown, so a change shows once the window manager applied it.
///
/// # Arguments
/// * `d` - The desktop.
/// * `now` - The current time.
pub fn tick(d: &mut Desktop, now: Instant) {
    if !d.settings.polls_keybinds() {
        return;
    }
    if d.settings.poll_at.is_none_or(|at| now >= at) {
        d.settings.reread_keybinds();
        d.settings.poll_at = Some(now + KEYBINDS_POLL);
        clamp_scroll(d);
    }
}

/// Keeps the page's scroll within its content, which shrinks when the
/// shortcuts read again are fewer.
fn clamp_scroll(d: &mut Desktop) {
    let frame = frame(d);
    let content = build(d, frame, 0.0, view(d)).content as f64;
    let max = (content - scroll_rect_in(frame, view(d)).h as f64).max(0.0);
    if d.settings.scroll > max {
        d.settings.scroll = max;
        d.redraw_settings();
    }
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
    d.redraw_settings();
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
            d.redraw_panel();
        }
        Field::Color => {
            match d.settings.monitor {
                None => d.config.wallpaper.color = color,
                Some(n) => d.config.monitors.entry(n).or_default().color = Some(color),
            }
            d.repaint_wallpapers();
        }
        Field::ClockFormat | Field::StartIcon | Field::StartLabel | Field::Image | Field::Command => {}
    }
}

/// The config key a color field writes.
fn color_key(d: &Desktop, field: Field) -> Option<&'static str> {
    match field {
        Field::Color => Some(wallpaper_key(d, "background")),
        Field::Theme(color) => Some(color.key()),
        Field::ClockFormat | Field::StartIcon | Field::StartLabel | Field::Image | Field::Command => None,
    }
}

/// Follows a drag of a slider, of a part of the color picker or of the
/// scrollbar, applying the value on screen.
///
/// # Arguments
/// * `d` - The desktop.
/// * `x` - Pointer X.
/// * `y` - Pointer Y.
fn drag(d: &mut Desktop, x: f64, y: f64) {
    if let Some(grab) = d.settings.bar_drag {
        if let Some(bar) = scrollbar(d) {
            d.settings.scroll = bar.scroll_at(y, grab);
        }
        d.redraw_settings();
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
    d.redraw_settings();
}

/// Ends a drag of a slider or of the color picker, writing the value.
///
/// # Arguments
/// * `d` - The desktop.
fn release(d: &mut Desktop) {
    if d.settings.bar_drag.take().is_some() {
        let (x, y) = d.settings.pointer;
        d.set_hover(hit(d, x, y).unwrap_or(Hover::None));
        d.redraw_settings();
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

/// Writes the focused field's text and drops the focus.  The command of a
/// shortcut being added is kept for its capture.
fn commit_field(d: &mut Desktop) {
    let Some(field) = d.settings.focus.take() else {
        return;
    };
    let text = std::mem::take(&mut d.settings.editing);
    let (key, value) = match field {
        Field::Command => {
            d.settings.command = text.trim().to_string();
            return;
        }
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
/// Whether any of those was open, so the key is used up.
fn escape(d: &mut Desktop) -> bool {
    if d.settings.focus.take().is_some() {
        d.settings.editing.clear();
    } else if d.settings.picker.take().is_some() {
    } else {
        return false;
    }
    let (x, y) = d.settings.pointer;
    d.set_hover(hit(d, x, y).unwrap_or(Hover::None));
    d.redraw_settings();
    true
}

/// Handles wheel scrolling over the page or the application list.
///
/// # Arguments
/// * `d` - The desktop.
/// * `x` - Pointer X.
/// * `y` - Pointer Y.
/// * `delta` - Scroll amount, positive downwards.
pub fn pointer_axis(d: &mut Desktop, x: f64, y: f64, delta: f64) {
    let frame = frame(d);
    let page = scroll_rect_in(frame, view(d));
    if x < page.x as f64 {
        return;
    }
    let content = build(d, frame, 0.0, view(d)).content as f64;
    let max = (content - page.h as f64).max(0.0);
    let next = (d.settings.scroll + delta.signum() * ui::SCROLL_STEP).clamp(0.0, max);
    if next != d.settings.scroll {
        d.settings.scroll = next;
        d.set_hover(hit(d, x, y).unwrap_or(Hover::None));
        d.redraw_settings();
    }
}

/// Handles a key press in the window: Escape closes what is open in it
/// and then the window itself; typing goes to the focused field, Enter
/// writes it.
///
/// # Arguments
/// * `d` - The desktop.
/// * `key` - The key.
pub fn key(d: &mut Desktop, key: Key) {
    if d.settings.capture.is_some() {
        return;
    }
    if key == Key::Escape {
        if !escape(d) {
            close(d);
        }
        return;
    }
    match d.settings.focus {
        None => return,
        Some(_) => match key {
            Key::Char(c) => d.settings.editing.push(c),
            Key::Backspace => {
                d.settings.editing.pop();
            }
            Key::Enter | Key::Tab => commit_field(d),
            _ => return,
        },
    }
    d.redraw_settings();
}

/// Closes the window, from its frame's close button or Escape.
pub fn close(d: &mut Desktop) {
    cancel_capture(d);
    if let Some(id) = d.settings.id {
        d.requests.push(Request::CloseWindow { id });
    }
    d.settings.close();
    if matches!(d.hover, Hover::Control(_)) {
        d.set_hover(Hover::None);
    }
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

    /// The labels of a menu's rows, separators left out.
    fn labels(entries: &[MenuEntry]) -> Vec<&'static str> {
        entries
            .iter()
            .filter_map(|entry| match entry {
                MenuEntry::Item { label, .. } => Some(*label),
                MenuEntry::Separator => None,
            })
            .collect()
    }

    #[test]
    fn each_style_matches_only_itself() {
        for style in &STYLES {
            let config = config_from(&style.changes());
            assert_eq!(current_style(&config), Some(style.name));
            assert_eq!(style.changes().len(), 8);
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
            assert_eq!(current_scheme(scheme_theme(colors)), Some(*name));
        }
        assert_eq!(current_scheme(ui::Theme::default()), Some("Dark"));
        assert_eq!(SCHEMES[0].0, "Dark");
        let defaults = &SCHEMES[0].1;
        assert_eq!(ACCENTS[0], defaults[0]);
        assert_eq!(&SURFACES[..2], &defaults[1..3]);
        assert_eq!(TEXTS[0], defaults[3]);
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

    #[test]
    fn page_ids_round_trip_and_reject_the_unknown() {
        for page in Page::ALL {
            assert_eq!(Page::from_id(page.id()), Some(page));
            assert_eq!(Page::from_id(&page.id().to_uppercase()), Some(page));
        }
        assert_eq!(Page::from_id(" wallpaper "), Some(Page::Wallpaper));
        assert_eq!(Page::from_id("sound"), None);
        assert_eq!(Page::from_id(""), None);
    }

    #[test]
    fn every_scheme_accent_takes_white_text() {
        for (name, colors) in &SCHEMES {
            let accent = parse_color(colors[0]).unwrap();
            assert_eq!(selkies_draw::style::on_color(accent), Color::WHITE, "{name}");
        }
    }

    #[test]
    fn a_groups_preset_cards_share_one_line_when_they_fit() {
        let page = scroll_rect_in(Rect::new(0, 0, WIDTH, HEIGHT), View::Page(Page::Theme));
        let mut out = Layout::default();
        let mut groups = Groups {
            out: &mut out,
            clip: page,
            x: page.x,
            width: page.w,
            y: page.y,
            card_top: None,
            rows: 0,
            started: false,
        };
        let card = |name: &str| PresetCard {
            name: name.to_string(),
            look: preview::Look {
                theme: ui::Theme::default(),
                style: crate::config::WindowStyle::default(),
                centered: false,
                margin: 0,
                bar_corners: 0,
                menu: MenuLayout::Columns,
            },
            action: Action::Reset,
            active: false,
        };
        groups.presets(SCHEMES.iter().map(|(name, _)| card(name)).collect());
        groups.presets(STYLES.iter().map(|style| card(style.name)).collect());
        groups.finish();
        let schemes = &out.controls[..SCHEMES.len()];
        let styles = &out.controls[SCHEMES.len()..];
        for line in [schemes, styles] {
            assert!(line.iter().all(|c| c.rect.y == line[0].rect.y));
            let inner_right = page.x + page.w - CARD_PAD;
            let slack = inner_right - line.last().unwrap().rect.right();
            assert!((0..line.len() as i32).contains(&slack), "{slack}");
            assert_eq!(line[0].rect.x, page.x + CARD_PAD);
            assert!(line[0].rect.w >= PRESET_MIN_WIDTH);
        }
        assert!(schemes[0].rect.w < styles[0].rect.w);
    }

    #[test]
    fn the_pinned_previews_leave_the_rows_a_scrolled_area_below_them() {
        let frame = Rect::new(0, 0, WIDTH, HEIGHT);
        let page = page_rect_in(frame);
        for (shown, pinned) in [
            (Page::Theme, true),
            (Page::Wallpaper, true),
            (Page::Shortcuts, false),
        ] {
            let rows = scroll_rect_in(frame, View::Page(shown));
            assert_eq!(rows.bottom(), page.bottom());
            assert_eq!(rows.y > page.y, pinned, "{shown:?}");
        }
        assert_eq!(scroll_rect_in(frame, View::List("favorites")), page);
    }

    #[test]
    fn the_published_shortcuts_are_polled_a_second_apart_only_on_their_page() {
        let mut state = SettingsState::default();
        let before = Instant::now();
        state.open(Some(Page::Shortcuts));
        assert_eq!(state.next_poll(), None, "a closed window polls nothing");
        state.id = Some(7);
        let due = state.next_poll().expect("the Shortcuts page polls");
        assert!(due >= before + KEYBINDS_POLL);
        state.page = Page::Theme;
        assert_eq!(state.next_poll(), None);
        state.close();
        assert_eq!(state.next_poll(), None);
    }

    #[test]
    fn a_capture_takes_a_chord_or_a_modifier_released_alone() {
        let none = Modifiers::default();
        let logo = Modifiers { logo: true, ..none };
        let ctrl = Modifiers { ctrl: true, ..none };
        let super_l = Keysym::Super_L.raw();
        let mut held = None;
        assert_eq!(capture_step(&mut held, super_l, none, true), Step::Wait);
        assert_eq!(
            capture_step(&mut held, super_l, logo, false),
            Step::Chord("Super_L".to_string())
        );

        let mut held = None;
        assert_eq!(capture_step(&mut held, super_l, none, true), Step::Wait);
        assert_eq!(
            capture_step(&mut held, Keysym::Left.raw(), logo, true),
            Step::Chord("W-Left".to_string())
        );

        let mut held = None;
        let control = Keysym::Control_L.raw();
        assert_eq!(capture_step(&mut held, control, none, true), Step::Wait);
        assert_eq!(capture_step(&mut held, super_l, ctrl, true), Step::Wait);
        assert_eq!(held, None, "a second modifier makes the first no chord alone");
        assert_eq!(capture_step(&mut held, super_l, logo, false), Step::Wait);
        assert_eq!(capture_step(&mut held, control, ctrl, false), Step::Wait);

        let mut held = None;
        assert_eq!(
            capture_step(&mut held, Keysym::Escape.raw(), none, true),
            Step::Cancel
        );
        assert_eq!(
            capture_step(&mut held, Keysym::BackSpace.raw(), none, true),
            Step::Chord(String::new())
        );
        assert_eq!(
            capture_step(&mut held, Keysym::Escape.raw(), ctrl, true),
            Step::Chord("C-Escape".to_string())
        );
        assert_eq!(capture_step(&mut held, Keysym::t.raw(), none, false), Step::Wait);
    }

    #[test]
    fn shortcut_rows_offer_a_reset_a_removal_and_name_a_shared_chord() {
        let published = keybinds::parse(
            "W-Left\tW-Left\tSuper+Left\tTile left\n\
             W-Left\tW-Up\tSuper+Left\tTile up\n\
             \tA-F4\t\tClose\n\
             execute\tW-e\tSuper+E\tfoot\n",
        );
        let rows = shortcut_rows(&published, None);
        assert_eq!(rows.len(), 4);
        assert!(rows[0].buttons.is_empty());
        assert!(matches!(&rows[0].note, Some((text, Tone::Warning)) if text.contains("Tile up")));
        assert!(matches!(&rows[1].buttons[..], [(_, Action::ResetBind(origin))] if origin == "W-Up"));
        assert_eq!(rows[2].chord, "");
        assert!(matches!(&rows[2].buttons[..], [(_, Action::ResetBind(origin))] if origin == "A-F4"));
        assert!(rows[2].note.is_none());
        assert!(matches!(
            &rows[3].buttons[..],
            [(_, Action::RemoveCustom { spec, command })] if spec == "W-e" && command == "foot"
        ));
        let capture = Capture {
            target: CaptureTarget::Rebind("A-F4".to_string()),
            held: None,
        };
        let rows = shortcut_rows(&published, Some(&capture));
        assert!(rows[2].capturing);
        assert!(matches!(&rows[2].note, Some((_, Tone::Hint))));
    }

    #[test]
    fn each_bar_item_gets_its_own_menu() {
        let facts = MenuFacts {
            top: false,
            autohide: false,
            desktops: 2,
            muted: Some(false),
            launcher: Some("foot".to_string()),
            dnd: false,
            notifications: true,
        };
        let bar = panel_menu_entries(PanelTarget::Bar, &facts);
        assert_eq!(
            labels(&bar),
            vec![
                "Settings…",
                "Move panel to top",
                "Auto-hide the panel",
                "Add a desktop",
                "Reload applications",
            ]
        );
        let hidden = MenuFacts {
            top: true,
            autohide: true,
            ..facts.clone()
        };
        assert!(labels(&panel_menu_entries(PanelTarget::Bar, &hidden)).contains(&"Move panel to bottom"));
        assert!(labels(&panel_menu_entries(PanelTarget::Bar, &hidden)).contains(&"Always show the panel"));

        let pager = panel_menu_entries(PanelTarget::Pager, &facts);
        assert_eq!(
            labels(&pager),
            vec!["Add a desktop", "Remove a desktop", "Desktop settings…"]
        );
        assert!(pager.contains(&MenuEntry::Separator));
        assert!(matches!(
            &pager[0],
            MenuEntry::Item { action: Action::Set(changes), enabled: true, .. }
                if changes == &vec![("desktops", "3".to_string())]
        ));
        assert!(matches!(
            &pager[1],
            MenuEntry::Item { action: Action::Set(changes), enabled: true, .. }
                if changes == &vec![("desktops", "1".to_string())]
        ));
        assert!(matches!(
            &pager[3],
            MenuEntry::Item {
                action: Action::OpenSettings(Some(Page::Desktops)),
                ..
            }
        ));
        let one = MenuFacts {
            desktops: 1,
            ..facts.clone()
        };
        assert!(matches!(
            &panel_menu_entries(PanelTarget::Pager, &one)[1],
            MenuEntry::Item { enabled: false, .. }
        ));
        let full = MenuFacts {
            desktops: config::MAX_DESKTOPS,
            ..facts.clone()
        };
        assert!(matches!(
            &panel_menu_entries(PanelTarget::Pager, &full)[0],
            MenuEntry::Item { enabled: false, .. }
        ));

        assert_eq!(
            labels(&panel_menu_entries(PanelTarget::Clock, &facts)),
            vec!["Clock settings…", "Hide the clock"]
        );
        let volume = panel_menu_entries(PanelTarget::Volume, &facts);
        assert_eq!(
            labels(&volume),
            vec!["Mute", "Hide the volume control", "Sound settings…"]
        );
        let muted = MenuFacts {
            muted: Some(true),
            ..facts.clone()
        };
        assert_eq!(
            labels(&panel_menu_entries(PanelTarget::Volume, &muted))[0],
            "Unmute"
        );
        let silent = MenuFacts {
            muted: None,
            ..facts.clone()
        };
        assert!(matches!(
            &panel_menu_entries(PanelTarget::Volume, &silent)[0],
            MenuEntry::Item { enabled: false, .. }
        ));

        let start = panel_menu_entries(PanelTarget::Start, &MenuFacts::default());
        assert_eq!(labels(&start), vec!["Panel settings…", "Edit favorites…"]);
        let launcher = panel_menu_entries(PanelTarget::Launcher(0), &facts);
        assert_eq!(
            labels(&launcher),
            vec!["Panel settings…", "Edit favorites…", "Remove from quick launch"]
        );
        assert!(matches!(
            &launcher[2],
            MenuEntry::Item { action: Action::RemoveLauncher(id), .. } if id == "foot"
        ));
        assert_eq!(
            labels(&panel_menu_entries(PanelTarget::Tray, &facts)),
            vec!["Hide the system tray", "Panel settings…"]
        );
        assert_eq!(
            labels(&panel_menu_entries(PanelTarget::ShowDesktop, &facts)),
            vec!["Hide this button", "Panel settings…"]
        );
        let bell = panel_menu_entries(PanelTarget::Notifications, &facts);
        assert_eq!(labels(&bell), vec!["Do not disturb", "Clear all"]);
        assert!(matches!(
            &bell[1],
            MenuEntry::Item {
                action: Action::ClearNotifications,
                enabled: true,
                ..
            }
        ));
        let quiet = MenuFacts {
            dnd: true,
            notifications: false,
            ..facts.clone()
        };
        let bell = panel_menu_entries(PanelTarget::Notifications, &quiet);
        assert_eq!(labels(&bell), vec!["Allow notifications", "Clear all"]);
        assert!(matches!(&bell[1], MenuEntry::Item { enabled: false, .. }));
    }

    #[test]
    fn every_language_code_is_known_to_the_translations() {
        for (code, name) in LANGUAGES {
            assert!(!name.is_empty());
            if code != "en" {
                assert_eq!(i18n::normalize(code), Some(code), "{code}");
            }
        }
    }
}
