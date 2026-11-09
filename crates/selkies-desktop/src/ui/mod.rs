//! The visual language shared by every part of the desktop: sizes, colors
//! and the small widgets drawn more than once.

pub mod background;
pub mod files;
pub mod launcher;
pub mod menu;
pub mod notifications;
pub mod panel;
pub mod popups;
pub mod preview;
pub mod settings;

use tiny_skia::{Color, Path, PathBuilder};

use crate::config::{Surface, WindowStyle};
use crate::geom::Rect;
use crate::render::{Align, Canvas, Fonts, rgb, rgba};
pub use selkies_draw::style::{blend, gloss, luminance, shifted, surface_shade_in, with_alpha};

/// Default height of the bar; the `panel_height` setting overrides it.
pub const PANEL_HEIGHT: i32 = 32;
/// Width of the start button.
pub const START_WIDTH: i32 = 44;
/// Icon size in the bar and the menu's application list.
pub const ICON_SIZE: i32 = 18;
/// Icon size of desktop launchers.
pub const DESKTOP_ICON_SIZE: i32 = 48;
/// Width of a desktop launcher cell.
pub const DESKTOP_CELL_WIDTH: i32 = 96;
/// Height of a desktop launcher cell.
pub const DESKTOP_CELL_HEIGHT: i32 = 104;
/// Space between the bar and a popup, and between popups and the screen edge.
pub const POPUP_MARGIN: i32 = 6;
/// The tallest popup area the panel surface must leave room for.
pub const POPUP_AREA_HEIGHT: i32 = 620;
/// Base font size of the UI.
pub const FONT_SIZE: f32 = 13.0;
/// Small font size for secondary text.
pub const FONT_SMALL: f32 = 11.5;
/// Pixels one wheel step scrolls a list.
pub const SCROLL_STEP: f64 = 48.0;
/// Milliseconds a second click may follow the first to count as a double click.
pub const DOUBLE_CLICK_MS: u32 = 400;
/// Milliseconds the pointer rests before a tooltip shows.
pub const TOOLTIP_DELAY_MS: u64 = 600;
/// Narrowest a file manager window may be made.
pub const WINDOW_MIN_WIDTH: i32 = 480;
/// Shortest a file manager window may be made.
pub const WINDOW_MIN_HEIGHT: i32 = 320;

pub use selkies_draw::style::Theme;

/// The theme in use.
static THEME: std::sync::RwLock<Option<Theme>> = std::sync::RwLock::new(None);
/// The window style in use, which the bar and the popups follow.
static STYLE: std::sync::RwLock<Option<WindowStyle>> = std::sync::RwLock::new(None);

/// Puts a window style in use for everything drawn from now on.
///
/// # Arguments
/// * `style` - The style.
pub fn set_style(style: WindowStyle) {
    *STYLE.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(style);
}

/// The window style in use.
pub fn style() -> WindowStyle {
    STYLE
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .unwrap_or_default()
}

/// Corner radius of popups, the one windows have.
pub fn popup_radius() -> f32 {
    style().corners as f32
}

/// Corner radius of a bar narrower than the screen: a little more than the
/// popups', none when they are square.
pub fn panel_radius() -> f32 {
    let corners = style().corners;
    if corners == 0 { 0.0 } else { corners as f32 + 2.0 }
}

/// The color of a surface at a point, for the surface style in use: the
/// base color flat, lightened towards the top for the gradient, and a
/// lighter upper half over a darker lower half for the gloss.
///
/// # Arguments
/// * `base` - The surface's color.
/// * `v` - Vertical position from 0 at the top to 1 at the bottom.
pub fn surface_shade(base: Color, v: f32) -> Color {
    surface_shade_in(style().surface, base, v)
}

/// Tallest the gloss band of a glossy surface grows, so a tall popup gets
/// a sheen along its top rather than a seam through its middle.
const GLOSS_BAND: i32 = 28;

/// Fills a rounded surface in its style: flat in one call, else shaded
/// from top to bottom.
///
/// # Arguments
/// * `canvas` - The target.
/// * `rect` - The surface.
/// * `radius` - Corner radius.
/// * `base` - The surface's color, its alpha kept.
pub fn fill_surface(canvas: &mut Canvas<'_>, rect: Rect, radius: f32, base: Color) {
    match style().surface {
        Surface::Flat => canvas.fill_rounded(rect, radius, base),
        Surface::Gradient => canvas.fill_rounded_shaded(rect, radius, |_, v| surface_shade(base, v)),
        Surface::Glossy => {
            let split = (GLOSS_BAND as f32 / rect.h.max(1) as f32).min(0.5);
            canvas.fill_rounded_shaded(rect, radius, |_, v| gloss(base, v, split));
        }
    }
}

/// Puts a theme in use for everything drawn from now on.
///
/// # Arguments
/// * `theme` - The colors.
pub fn set_theme(theme: Theme) {
    *THEME.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(theme);
}

/// The theme in use.
pub fn theme() -> Theme {
    THEME
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .unwrap_or_default()
}

/// Bar background at an opacity in percent.
///
/// # Arguments
/// * `opacity` - Opacity from 0 to 100.
pub fn panel_bg(opacity: i32) -> Color {
    with_alpha(theme().panel, (opacity.clamp(0, 100) * 255 / 100) as u8)
}
/// Popup background.
pub fn popup_bg() -> Color {
    theme().popup
}
/// Popup outline.
pub fn popup_border() -> Color {
    with_alpha(theme().text, 0x24)
}
/// Raised surface inside a popup, such as a card or the search box.
pub fn field_bg() -> Color {
    with_alpha(theme().text, 0x0c)
}
/// Hover highlight.
pub fn hover_bg() -> Color {
    with_alpha(theme().text, 0x18)
}
/// Pressed or selected highlight.
pub fn active_bg() -> Color {
    with_alpha(theme().accent, 0x48)
}
/// Tooltip background.
pub fn tooltip_bg() -> Color {
    with_alpha(shifted(theme().popup, 14), 0xfa)
}
/// Accent, for markers, selections, switches and the active choice.
pub fn accent() -> Color {
    theme().accent
}
/// The accent at an alpha.
///
/// # Arguments
/// * `alpha` - Opacity from 0 to 255.
pub fn accent_alpha(alpha: u8) -> Color {
    with_alpha(theme().accent, alpha)
}
/// Primary text.
pub fn text() -> Color {
    theme().text
}
/// Secondary text.
pub fn text_dim() -> Color {
    with_alpha(theme().text, 0xa8)
}
/// Separator lines.
pub fn separator() -> Color {
    with_alpha(theme().text, 0x1c)
}
/// The text color at an alpha.
///
/// # Arguments
/// * `alpha` - Opacity from 0 to 255.
pub fn text_alpha(alpha: u8) -> Color {
    with_alpha(theme().text, alpha)
}
/// Whether the theme's text is light, so what contrasts with it is dark.
pub fn light_text() -> bool {
    luminance(theme().text) > 0.5
}

/// A shadow under text drawn over the wallpaper: dark under light text,
/// light under dark text, so the text reads either way.
pub fn text_shadow() -> Color {
    if light_text() {
        rgba(0, 0, 0, 0xa0)
    } else {
        rgba(0xff, 0xff, 0xff, 0xa0)
    }
}

/// Text drawn over the accent: black on a light accent, white on a dark one.
pub fn on_accent() -> Color {
    let c = theme().accent.to_color_u8();
    let luma = 0.299 * c.red() as f32 + 0.587 * c.green() as f32 + 0.114 * c.blue() as f32;
    if luma > 160.0 {
        rgb(0x10, 0x10, 0x10)
    } else {
        rgb(0xff, 0xff, 0xff)
    }
}

/// Width of the band at the right edge of scrolled content where its
/// scrollbar sits, wide enough for a finger.
pub const SCROLLBAR: i32 = 14;
/// Least height of a scrollbar's thumb.
const THUMB_MIN: i32 = 28;
/// Space between a scrollbar's thumb and the ends of its track.
const THUMB_INSET: i32 = 2;

/// A vertical scrollbar over the right edge of an area of scrolled content:
/// a thumb the pointer drags, on a track that is not drawn.
#[derive(Clone, Copy, Debug)]
pub struct Scrollbar {
    /// The band a press lands in, the whole height of the area.
    pub track: Rect,
    /// The thumb, placed for the current scroll.
    pub thumb: Rect,
    /// How far the content scrolls at most.
    max: f64,
}

impl Scrollbar {
    /// The scrollbar of an area, or `None` when its content fits.
    ///
    /// # Arguments
    /// * `area` - The area the content scrolls in.
    /// * `content` - Height of the content.
    /// * `scroll` - The current scroll offset.
    pub fn new(area: Rect, content: i32, scroll: f64) -> Option<Scrollbar> {
        let max = (content - area.h) as f64;
        let run = area.h - 2 * THUMB_INSET;
        if max <= 0.0 || run <= 0 {
            return None;
        }
        let track = Rect::new(area.right() - SCROLLBAR, area.y, SCROLLBAR, area.h);
        let thumb_h = ((run as f64 * area.h as f64 / content as f64) as i32).clamp(THUMB_MIN.min(run), run);
        let travel = (run - thumb_h) as f64;
        let y = track.y + THUMB_INSET + (travel * (scroll / max).clamp(0.0, 1.0)).round() as i32;
        let thumb = Rect::new(track.x + 3, y, SCROLLBAR - 6, thumb_h);
        Some(Scrollbar { track, thumb, max })
    }

    /// Where in the thumb a press grabs it: the pointer's offset when the
    /// press is on the thumb, its middle otherwise so the thumb jumps under
    /// the pointer.
    ///
    /// # Arguments
    /// * `y` - Pointer Y.
    pub fn grab(&self, y: f64) -> f64 {
        if y >= self.thumb.y as f64 && y < self.thumb.bottom() as f64 {
            y - self.thumb.y as f64
        } else {
            self.thumb.h as f64 / 2.0
        }
    }

    /// The scroll offset that puts the grabbed point of the thumb at a
    /// pointer Y.
    ///
    /// # Arguments
    /// * `y` - Pointer Y.
    /// * `grab` - The offset into the thumb the press grabbed.
    pub fn scroll_at(&self, y: f64, grab: f64) -> f64 {
        let travel = (self.track.h - 2 * THUMB_INSET - self.thumb.h) as f64;
        if travel <= 0.0 {
            return 0.0;
        }
        let top = y - grab - (self.track.y + THUMB_INSET) as f64;
        (top / travel * self.max).clamp(0.0, self.max)
    }

    /// Draws the thumb, stronger while the pointer is on it or drags it.
    ///
    /// # Arguments
    /// * `canvas` - The target.
    /// * `active` - Whether the thumb is hovered or dragged.
    pub fn draw(&self, canvas: &mut Canvas<'_>, active: bool) {
        let radius = self.thumb.w as f32 / 2.0;
        canvas.fill_rounded(self.thumb, radius, text_alpha(if active { 0x80 } else { 0x48 }));
    }
}

/// Draws a popup's rounded background with its border.
///
/// # Arguments
/// * `canvas` - The target.
/// * `rect` - The popup's rectangle.
pub fn draw_popup_frame(canvas: &mut Canvas<'_>, rect: Rect) {
    let radius = popup_radius();
    fill_surface(canvas, rect, radius, popup_bg());
    canvas.stroke_rounded(rect, radius, 1.0, popup_border());
}

/// Shapes text so it fits a width, replacing the tail with an ellipsis when it
/// does not.
///
/// # Arguments
/// * `fonts` - The font system.
/// * `text` - The text.
/// * `px` - Font size in physical pixels.
/// * `bold` - Whether to use the bold weight.
/// * `max_width` - Width limit in physical pixels.
pub fn shape_fit(
    fonts: &mut Fonts,
    text: &str,
    px: f32,
    bold: bool,
    max_width: f32,
) -> std::sync::Arc<crate::render::Text> {
    let full = fonts.shape(text, px, bold);
    if full.width <= max_width || max_width <= 0.0 {
        return full;
    }
    let chars: Vec<char> = text.chars().collect();
    let mut lo = 0usize;
    let mut hi = chars.len();
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let candidate: String = chars[..mid].iter().collect::<String>().trim_end().to_string() + "…";
        if fonts.shape(&candidate, px, bold).width <= max_width {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let candidate: String = chars[..lo].iter().collect::<String>().trim_end().to_string() + "…";
    fonts.shape(&candidate, px, bold)
}

/// Draws a single line of text fitted into a box.
///
/// # Arguments
/// * `canvas` - The target.
/// * `fonts` - The font system.
/// * `text` - The text.
/// * `rect` - The logical box; text is vertically centered in it.
/// * `size` - Font size in logical pixels.
/// * `bold` - Whether to use the bold weight.
/// * `align` - Horizontal alignment.
/// * `color` - Text color.
#[allow(clippy::too_many_arguments)]
pub fn label(
    canvas: &mut Canvas<'_>,
    fonts: &mut Fonts,
    text: &str,
    rect: Rect,
    size: f32,
    bold: bool,
    align: Align,
    color: Color,
) {
    if rect.is_empty() || text.is_empty() {
        return;
    }
    let scale = canvas.scale() as f32;
    let shaped = shape_fit(fonts, text, size * scale, bold, rect.w as f32 * scale);
    canvas.draw_text(fonts, &shaped, rect.x, rect.y + rect.h / 2, rect.w, align, color);
}

/// Splits a name over at most two lines at word boundaries, the second
/// ellipsized when it still does not fit.
///
/// # Arguments
/// * `fonts` - The font system.
/// * `text` - The text.
/// * `px` - Font size in physical pixels.
/// * `max_width` - Width limit in physical pixels.
pub fn wrap_two_lines(
    fonts: &mut Fonts,
    text: &str,
    px: f32,
    max_width: f32,
) -> (
    std::sync::Arc<crate::render::Text>,
    Option<std::sync::Arc<crate::render::Text>>,
) {
    let whole = fonts.shape(text, px, false);
    if whole.width <= max_width {
        return (whole, None);
    }
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut split = 0;
    for i in 1..words.len() {
        let head = words[..i].join(" ");
        if fonts.shape(&head, px, false).width <= max_width {
            split = i;
        } else {
            break;
        }
    }
    if split == 0 {
        return (shape_fit(fonts, text, px, false, max_width), None);
    }
    let first = words[..split].join(" ");
    let rest = words[split..].join(" ");
    let line1 = fonts.shape(&first, px, false);
    let line2 = shape_fit(fonts, &rest, px, false, max_width);
    (line1, Some(line2))
}

/// Width in logical pixels a line of text takes.
///
/// # Arguments
/// * `fonts` - The font system.
/// * `text` - The text.
/// * `size` - Font size in logical pixels.
/// * `bold` - Whether to use the bold weight.
/// * `scale` - The output scale.
pub fn text_width(fonts: &mut Fonts, text: &str, size: f32, bold: bool, scale: i32) -> i32 {
    (fonts.shape(text, size * scale as f32, bold).width / scale as f32).ceil() as i32
}

/// A speaker glyph with waves for the volume level, as a path in logical
/// coordinates inside a box.
///
/// # Arguments
/// * `rect` - The box the glyph is centered in.
/// * `level` - Volume from 0 to 100; selects how many waves are drawn.
/// * `muted` - Whether to draw the mute cross instead of waves.
pub fn speaker_path(rect: Rect, level: u8, muted: bool) -> (Path, Option<Path>) {
    let cx = rect.x as f32 + rect.w as f32 / 2.0;
    let cy = rect.y as f32 + rect.h as f32 / 2.0;
    let mut pb = PathBuilder::new();
    pb.move_to(cx - 7.0, cy - 2.5);
    pb.line_to(cx - 4.0, cy - 2.5);
    pb.line_to(cx - 0.5, cy - 6.0);
    pb.line_to(cx - 0.5, cy + 6.0);
    pb.line_to(cx - 4.0, cy + 2.5);
    pb.line_to(cx - 7.0, cy + 2.5);
    pb.close();
    let body = pb
        .finish()
        .unwrap_or_else(|| PathBuilder::from_rect(tiny_skia::Rect::from_xywh(cx, cy, 1.0, 1.0).unwrap()));
    let mut waves = PathBuilder::new();
    if muted {
        waves.move_to(cx + 2.0, cy - 3.0);
        waves.line_to(cx + 7.0, cy + 3.0);
        waves.move_to(cx + 7.0, cy - 3.0);
        waves.line_to(cx + 2.0, cy + 3.0);
    } else {
        if level > 0 {
            waves.move_to(cx + 2.0, cy - 2.5);
            waves.quad_to(cx + 4.0, cy, cx + 2.0, cy + 2.5);
        }
        if level > 33 {
            waves.move_to(cx + 4.0, cy - 5.0);
            waves.quad_to(cx + 7.5, cy, cx + 4.0, cy + 5.0);
        }
        if level > 66 {
            waves.move_to(cx + 6.0, cy - 7.5);
            waves.quad_to(cx + 11.0, cy, cx + 6.0, cy + 7.5);
        }
    }
    (body, waves.finish())
}

/// A cross centered in a box, for close buttons.
///
/// # Arguments
/// * `rect` - The box.
/// * `reach` - How far each stroke reaches from the center.
pub fn cross_path(rect: Rect, reach: f32) -> Option<Path> {
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

/// A chevron pointing right, for category rows.
///
/// # Arguments
/// * `x` - Logical X of the chevron's left.
/// * `cy` - Logical Y of its vertical center.
pub fn chevron_path(x: i32, cy: i32) -> Option<Path> {
    let mut pb = PathBuilder::new();
    pb.move_to(x as f32, cy as f32 - 4.0);
    pb.line_to(x as f32 + 4.0, cy as f32);
    pb.line_to(x as f32, cy as f32 + 4.0);
    pb.finish()
}

/// A magnifying glass for the search box.
///
/// # Arguments
/// * `cx` - Logical X of the glass center.
/// * `cy` - Logical Y of the glass center.
pub fn search_path(cx: i32, cy: i32) -> Option<Path> {
    let mut pb = PathBuilder::new();
    pb.push_circle(cx as f32 - 1.5, cy as f32 - 1.5, 5.0);
    pb.move_to(cx as f32 + 2.5, cy as f32 + 2.5);
    pb.line_to(cx as f32 + 6.5, cy as f32 + 6.5);
    pb.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrollbar_tracks_the_content() {
        let area = Rect::new(0, 0, 100, 200);
        assert!(Scrollbar::new(area, 150, 0.0).is_none());
        let bar = Scrollbar::new(area, 4000, 0.0).unwrap();
        assert_eq!(bar.thumb.h, THUMB_MIN);
        assert_eq!(bar.thumb.y, area.y + THUMB_INSET);
        assert_eq!(bar.track.x, area.right() - SCROLLBAR);
        let end = Scrollbar::new(area, 4000, 3800.0).unwrap();
        assert_eq!(end.thumb.bottom(), area.bottom() - THUMB_INSET);
        let mid = Scrollbar::new(area, 400, 100.0).unwrap();
        let y = mid.thumb.y as f64 + 5.0;
        let grab = mid.grab(y);
        assert_eq!(grab, 5.0);
        assert!((mid.scroll_at(y, grab) - 100.0).abs() < 1.0);
        let outside = mid.grab(1.0);
        assert_eq!(outside, mid.thumb.h as f64 / 2.0);
        assert_eq!(mid.scroll_at(-50.0, grab), 0.0);
        assert_eq!(mid.scroll_at(1000.0, grab), 200.0);
    }

    #[test]
    fn surfaces_shade_from_their_base() {
        let base = rgba(0x40, 0x40, 0x40, 0x80);
        assert_eq!(surface_shade_in(Surface::Flat, base, 0.5), base);
        let top = surface_shade_in(Surface::Gradient, base, 0.0);
        let bottom = surface_shade_in(Surface::Gradient, base, 1.0);
        assert!(luminance(top) > luminance(bottom));
        assert_eq!(bottom.to_color_u8(), base.to_color_u8());
        assert_eq!(top.alpha(), base.alpha());
        let band = surface_shade_in(Surface::Glossy, base, 0.49);
        let below = surface_shade_in(Surface::Glossy, base, 0.5);
        assert!(luminance(band) > luminance(below));
        assert!(luminance(gloss(base, 0.0, 0.1)) > luminance(gloss(base, 0.1, 0.1)));
        assert_eq!(gloss(base, 0.1, 0.1).to_color_u8(), base.to_color_u8());
    }

    #[test]
    fn derived_colors_read_on_their_backgrounds() {
        let white = rgb(0xff, 0xff, 0xff);
        let black = rgb(0, 0, 0);
        assert_eq!(blend(black, white, 0.0).to_color_u8(), black.to_color_u8());
        assert_eq!(blend(black, white, 1.0).to_color_u8(), white.to_color_u8());
        assert_eq!(
            blend(rgba(0, 0, 0, 0x80), white, 0.5).alpha(),
            rgba(0, 0, 0, 0x80).alpha()
        );
        assert!(luminance(white) > 0.99 && luminance(black) < 0.01);
        assert_eq!(
            shifted(rgb(0xf0, 0x10, 0x80), 40).to_color_u8(),
            rgb(0xff, 0x38, 0xa8).to_color_u8()
        );
        set_theme(Theme {
            accent: rgb(0xf5, 0xf5, 0x60),
            text: rgb(0x10, 0x10, 0x10),
            ..Theme::default()
        });
        assert!(!light_text());
        assert_eq!(on_accent().to_color_u8(), rgb(0x10, 0x10, 0x10).to_color_u8());
        assert!(luminance(text_shadow()) > 0.5);
        set_theme(Theme::default());
        assert!(light_text());
        assert_eq!(on_accent().to_color_u8(), Color::WHITE.to_color_u8());
        assert!(luminance(text_shadow()) < 0.5);
    }

    #[test]
    fn radii_follow_the_style() {
        set_style(WindowStyle {
            corners: 0,
            ..WindowStyle::default()
        });
        assert_eq!(popup_radius(), 0.0);
        assert_eq!(panel_radius(), 0.0);
        set_style(WindowStyle {
            corners: 6,
            ..WindowStyle::default()
        });
        assert_eq!(popup_radius(), 6.0);
        assert_eq!(panel_radius(), 8.0);
        set_style(WindowStyle::default());
    }
}
