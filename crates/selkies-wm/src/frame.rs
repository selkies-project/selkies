/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Server-side window frames: their layout around a client area, the hit test that names what a
//! point on them means, and their rasterization through `selkies-draw` into the parts a backend
//! presents.
//!
//! A frame is a title bar over the client and a border down its other three sides, drawn as one
//! slab with a hairline outline: the border takes the title bar's base color rather than a color
//! of its own, a 1 px outline in the border color runs along the frame's whole outer edge,
//! following the title bar's rounded top corners and the bottom border's rounded bottom corners,
//! and the title bar ends in a hairline halfway between the two colors rather than a bar. The
//! title bar carries the window's icon, its title, and the iconify, maximize, and close buttons
//! on the side the desktop's window style names, drawn as line glyphs, as colored circles, or as
//! wide flat buttons; a hovered line or wide button is a filled circle behind its glyph, red
//! under a white glyph for close and a translucent wash of the text color for the others, deeper
//! while it is held. The title bar's top corners are
//! rounded by the style's radius, as the shell's popups are, unless the window is maximized. The
//! border is the resize handle: its corners pull two edges, its sides one, and the title bar's
//! top rows pull the top edge, so a frame of a few pixels still resizes from every side.
//!
//! A window that is not maximized casts a soft shadow when the backend asks for one: stacked rounded
//! rectangles, offset downward, whose opacity falls off quadratically from the frame's edge
//! ([`Shadow`]). Four strips around the frame carry it, each its own raster no larger than the
//! strip; the title bar and the bottom border paint it beneath their own content too, so the
//! pixels their rounded corners leave uncovered show the shadow rather than a notch in it. The
//! strips lie outside the frame's extents and never change them, so they take no input and move
//! nothing the client is told. Only the Wayland compositor asks: it presents a part wherever the
//! part is placed and blends it over what lies beneath, while the X server composites nothing
//! under a window and the frame window clips every part to its extents.
//!
//! Every part is rasterized at the output's integer scale and placed relative to the client
//! area's top-left corner, so a backend presents it without knowing what it shows. A frame is
//! drawn again only when something it shows changed (the title, the focus, a hovered button, the
//! width), never per frame of the stream.

use std::sync::Arc;

use pixelflux::wm::{Extents, Frame, FramePart, Image, Rect, Size};
use selkies_draw::geom::Rect as DrawRect;
use selkies_draw::icons::Icons;
use selkies_draw::render::{Align, Canvas, Fonts};
use selkies_draw::style::{ButtonSide, Glyphs, Surface, WindowStyle, blend, shifted, with_alpha};
use selkies_draw::tiny_skia::{Color, PathBuilder, Pixmap};

use crate::config::Context;
use crate::theme::Look;

/// Height of the title bar in logical pixels.
pub const TITLE_HEIGHT: i32 = 30;
/// Thickness of the left, right, and bottom borders in logical pixels.
pub const BORDER: i32 = 4;
/// Rows at the top of the title bar that pull the top edge rather than move the window.
const TOP_GRIP: i32 = 4;
/// Pixels from a frame's corner that pull two edges at once.
const CORNER: i32 = 24;
/// Side of a square line-glyph button.
const BUTTON: i32 = 22;
/// Diameter of a circle button.
const CIRCLE: i32 = 13;
/// Width of a wide flat button.
const WIDE_BUTTON: i32 = 40;
/// Side of the title bar's icon.
const ICON: i32 = 16;
/// Space between the title bar's edge and its first element.
const PADDING: i32 = 8;
/// The title's font size in logical pixels.
const TITLE_FONT: f32 = 13.0;
/// Logical pixels the shadow spreads beyond the rectangle that casts it.
const SHADOW: i32 = 24;
/// Logical pixels the shadow's rectangle sits below the frame's.
const SHADOW_OFFSET: i32 = 6;
/// Opacity of a focused window's shadow under its frame, before it falls off.
const SHADOW_ACTIVE: f32 = 0.45;
/// Opacity of an unfocused window's shadow under its frame.
const SHADOW_INACTIVE: f32 = 0.25;

/// Where a frame's parts and controls lie, relative to the frame's top-left corner, for a client
/// area of a size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub extents: Extents,
    /// The frame's size, the client grown by the extents.
    pub size: Size,
    pub icon: Option<Rect>,
    /// The buttons in drawing order: the context each stands for and its rectangle.
    pub buttons: Vec<(Context, Rect)>,
    /// The title's rectangle, between the icon and the buttons.
    pub title: Rect,
}

/// The extents a frame adds around a client: the title bar and the border, the border alone on a
/// maximized window, nothing while undecorated.
pub fn extents(style: &WindowStyle, maximized: bool, shaded: bool) -> Extents {
    let _ = style;
    Extents {
        top: TITLE_HEIGHT,
        left: if maximized { 0 } else { BORDER },
        right: if maximized { 0 } else { BORDER },
        bottom: if maximized || shaded { 0 } else { BORDER },
    }
}

/// Which buttons a title bar shows, the image's choice over the theme's three.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Buttons {
    pub minimize: bool,
    pub maximize: bool,
    pub close: bool,
}

impl Buttons {
    /// Every button.
    pub const ALL: Buttons = Buttons {
        minimize: true,
        maximize: true,
        close: true,
    };

    /// The buttons a comma-separated list names (`minimize`, `maximize`, `close`, openbox's
    /// `iconify` too); `none`, or a list naming none of them, for no buttons.
    pub fn parse(text: &str) -> Buttons {
        let mut buttons = Buttons {
            minimize: false,
            maximize: false,
            close: false,
        };
        for word in text.split(',').map(|w| w.trim().to_ascii_lowercase()) {
            match word.as_str() {
                "minimize" | "iconify" => buttons.minimize = true,
                "maximize" => buttons.maximize = true,
                "close" => buttons.close = true,
                _ => {}
            }
        }
        buttons
    }

    /// Whether the button of a context shows.
    pub fn shows(self, context: Context) -> bool {
        match context {
            Context::Iconify => self.minimize,
            Context::Maximize => self.maximize,
            Context::Close => self.close,
            _ => true,
        }
    }
}

/// The layout of a frame around a client of `client` logical pixels, with the title bar's
/// buttons the image shows; the title takes the room a hidden button leaves.
pub fn layout(style: &WindowStyle, client: Size, maximized: bool, shaded: bool, shown: Buttons) -> Layout {
    let extents = extents(style, maximized, shaded);
    let width = client.w + extents.left + extents.right;
    let height = if shaded {
        extents.top + extents.bottom
    } else {
        client.h + extents.top + extents.bottom
    };
    let (side, step) = match style.glyphs {
        Glyphs::Lines => (BUTTON, BUTTON + 2),
        Glyphs::Circles => (CIRCLE, CIRCLE + 7),
        Glyphs::Wide => (WIDE_BUTTON, WIDE_BUTTON),
    };
    let button_h = if style.glyphs == Glyphs::Wide {
        TITLE_HEIGHT
    } else {
        side
    };
    let top = (TITLE_HEIGHT - button_h) / 2;
    let order: Vec<Context> = match style.buttons {
        ButtonSide::Right => [Context::Iconify, Context::Maximize, Context::Close],
        ButtonSide::Left => [Context::Close, Context::Iconify, Context::Maximize],
    }
    .into_iter()
    .filter(|&c| shown.shows(c))
    .collect();
    let mut buttons = Vec::new();
    let pad = if style.glyphs == Glyphs::Wide { 0 } else { PADDING };
    match style.buttons {
        ButtonSide::Right => {
            let mut x = width - pad - side;
            for context in order.iter().rev() {
                buttons.push((*context, Rect::new(x, top, side, button_h)));
                x -= step;
            }
            buttons.reverse();
        }
        ButtonSide::Left => {
            let mut x = pad;
            for &context in &order {
                buttons.push((context, Rect::new(x, top, side, button_h)));
                x += step;
            }
        }
    }
    let buttons_w = if order.is_empty() {
        0
    } else {
        order.len() as i32 * step + pad
    };
    let icon = match style.buttons {
        ButtonSide::Right => Rect::new(PADDING, (TITLE_HEIGHT - ICON) / 2, ICON, ICON),
        ButtonSide::Left => Rect::new(width - PADDING - ICON, (TITLE_HEIGHT - ICON) / 2, ICON, ICON),
    };
    let (title_x, title_right) = match style.buttons {
        ButtonSide::Right => (icon.right() + PADDING, width - buttons_w - PADDING),
        ButtonSide::Left => (buttons_w + PADDING, icon.x - PADDING),
    };
    Layout {
        extents,
        size: Size { w: width, h: height },
        icon: Some(icon),
        buttons,
        title: Rect::new(title_x, 0, (title_right - title_x).max(0), TITLE_HEIGHT),
    }
}

/// What a point on the frame means, for a point relative to the frame's top-left corner that
/// lies outside the client area; `None` for a point inside the client or off the frame.
pub fn hit(layout: &Layout, x: i32, y: i32, shaded: bool) -> Option<Context> {
    let Size { w, h } = layout.size;
    if x < 0 || y < 0 || x >= w || y >= h {
        return None;
    }
    let e = layout.extents;
    if y >= e.top && y < h - e.bottom && x >= e.left && x < w - e.right && !shaded {
        return None;
    }
    if y < e.top {
        if y < TOP_GRIP && !shaded {
            return Some(if x < CORNER {
                Context::TLCorner
            } else if x >= w - CORNER {
                Context::TRCorner
            } else {
                Context::Top
            });
        }
        if e.left > 0 && (x < e.left || x >= w - e.right) {
            return Some(if x < e.left {
                Context::TLCorner
            } else {
                Context::TRCorner
            });
        }
        for (context, rect) in &layout.buttons {
            if rect.contains(x, y) {
                return Some(*context);
            }
        }
        if layout.icon.is_some_and(|r| r.contains(x, y)) {
            return Some(Context::Icon);
        }
        return Some(Context::Titlebar);
    }
    if y >= h - e.bottom {
        return Some(if x < CORNER {
            Context::BLCorner
        } else if x >= w - CORNER {
            Context::BRCorner
        } else {
            Context::Bottom
        });
    }
    if x < e.left {
        return Some(if y >= h - CORNER {
            Context::BLCorner
        } else {
            Context::Left
        });
    }
    Some(if y >= h - CORNER {
        Context::BRCorner
    } else {
        Context::Right
    })
}

/// What a frame shows, the input of one rasterization.
#[derive(Clone, Debug, PartialEq)]
pub struct Appearance<'a> {
    pub title: &'a str,
    /// The window's own icon, else the icon theme is asked by `app_id`.
    pub icon: Option<&'a Image>,
    pub app_id: &'a str,
    pub active: bool,
    pub maximized: bool,
    pub shaded: bool,
    /// The button the pointer rests on, drawn highlighted.
    pub hover: Option<Context>,
    /// The button held down.
    pub pressed: Option<Context>,
    /// The output's integer scale the parts are drawn at.
    pub scale: i32,
    /// Whether the frame casts a shadow outside its extents, for a backend that blends the parts
    /// over what lies beneath them; a maximized window casts none either way.
    pub shadow: bool,
    /// The title bar's buttons.
    pub buttons: Buttons,
}

/// The rasterizer of frames: the font system and the icon lookup, kept across frames so text
/// shaping and icon files are cached.
pub struct Renderer {
    pub fonts: Fonts,
    pub icons: Icons,
}

impl Renderer {
    /// A renderer over the system fonts and the named icon theme, or the detected one.
    pub fn new(icon_theme: Option<&str>) -> Renderer {
        Renderer {
            fonts: Fonts::new(),
            icons: Icons::new(icon_theme),
        }
    }

    /// Switch icon themes.
    pub fn set_icon_theme(&mut self, icon_theme: Option<&str>) {
        self.icons = Icons::new(icon_theme);
    }

    /// The frame around a client of `client` logical pixels, every part drawn: the title bar,
    /// the left and right borders, the bottom border, and then the shadow's strips when the
    /// appearance asks for them.
    pub fn render(&mut self, look: &Look, client: Size, appearance: &Appearance<'_>) -> Frame {
        let style = &look.style;
        let palette = &look.palette;
        let layout = layout(
            style,
            client,
            appearance.maximized,
            appearance.shaded,
            appearance.buttons,
        );
        let e = layout.extents;
        let Size { w, h } = layout.size;
        let scale = appearance.scale.max(1);
        let mut parts = Vec::new();
        let origin_x = -e.left;
        let origin_y = -e.top;
        let shadow = Shadow::cast(appearance, style, Rect::new(origin_x, origin_y, w, h));
        if let Some(image) = self.render_titlebar(look, &layout, appearance, shadow.as_ref(), scale) {
            parts.push(FramePart {
                rect: Rect::new(origin_x, origin_y, w, e.top),
                image,
            });
        }
        let (base, border) = if appearance.active {
            (palette.title_active, palette.border_active)
        } else {
            (palette.title_inactive, palette.border_inactive)
        };
        let radius = if appearance.maximized {
            0.0
        } else {
            style.corners as f32
        };
        let body_h = h - e.top;
        if e.left > 0 && body_h > 0 {
            let side_h = body_h - e.bottom;
            parts.push(FramePart {
                rect: Rect::new(origin_x, 0, e.left, side_h),
                image: render_side(e.left, side_h, scale, base, border, 0),
            });
            parts.push(FramePart {
                rect: Rect::new(client.w, 0, e.right, side_h),
                image: render_side(e.right, side_h, scale, base, border, e.right - 1),
            });
        }
        if e.bottom > 0 {
            let rect = Rect::new(origin_x, h - e.top - e.bottom, w, e.bottom);
            let under = shadow.as_ref().map(|shadow| (shadow, rect.x, rect.y));
            parts.push(FramePart {
                rect,
                image: render_bottom(w, e.bottom, scale, base, border, radius, under),
            });
        }
        if let Some(shadow) = &shadow {
            parts.extend(render_shadow(shadow, scale));
        }
        Frame { extents: e, parts }
    }

    /// The title bar, over the shadow when the frame casts one.
    fn render_titlebar(
        &mut self,
        look: &Look,
        layout: &Layout,
        appearance: &Appearance<'_>,
        shadow: Option<&Shadow>,
        scale: i32,
    ) -> Option<Image> {
        let style = &look.style;
        let palette = &look.palette;
        let w = layout.size.w;
        let h = layout.extents.top;
        let mut pixmap = Pixmap::new((w * scale) as u32, (h * scale) as u32)?;
        {
            let mut canvas = Canvas::new(pixmap.as_mut(), scale);
            if let Some(shadow) = shadow {
                shadow.paint(&mut canvas, -layout.extents.left, -layout.extents.top);
            }
            let active = appearance.active;
            let radius = if appearance.maximized {
                0.0
            } else {
                (style.corners as f32).min(h as f32 / 2.0)
            };
            let bar = DrawRect::new(0, 0, w, h);
            let base = if active {
                palette.title_active
            } else {
                palette.title_inactive
            };
            match style.surface {
                Surface::Flat => canvas.fill_rounded(bar, radius, base),
                surface => {
                    canvas.fill_rounded_shaded(bar, radius, |_, v| palette.title_shade(surface, active, v))
                }
            }
            if radius > 0.0 {
                let bottom = DrawRect::new(0, h / 2, w, h - h / 2);
                canvas.fill_rect(bottom, base);
                if style.surface != Surface::Flat {
                    let half = h / 2;
                    for row in half..h {
                        let v = row as f32 / h as f32;
                        canvas.fill_rect(
                            DrawRect::new(0, row, w, 1),
                            palette.title_shade(style.surface, active, v),
                        );
                    }
                }
            }
            let border = if active {
                palette.border_active
            } else {
                palette.border_inactive
            };
            let separator = if appearance.shaded {
                border
            } else {
                blend(base, border, 0.5)
            };
            canvas.fill_rect(DrawRect::new(0, h - 1, w, 1), separator);
            if !appearance.maximized {
                let past_bottom = radius.ceil() as i32 + 2;
                canvas.stroke_rounded(DrawRect::new(0, 0, w, h + past_bottom), radius, 1.0, border);
            }
            let label = if active {
                palette.label_active
            } else {
                palette.label_inactive
            };
            if let Some(icon_rect) = layout.icon
                && let Some(icon) = self.icon_pixmap(appearance, (ICON * scale) as u32)
            {
                canvas.draw_image_fit(&icon, draw_rect(icon_rect));
            }
            let title_rect = draw_rect(layout.title);
            if !appearance.title.is_empty() && title_rect.w > 0 {
                let shaped = shape_fit(
                    &mut self.fonts,
                    appearance.title,
                    TITLE_FONT * scale as f32,
                    title_rect.w as f32 * scale as f32,
                );
                let align = if style.glyphs == Glyphs::Wide {
                    Align::Left
                } else {
                    Align::Center
                };
                canvas.draw_text(
                    &mut self.fonts,
                    &shaped,
                    title_rect.x,
                    title_rect.y + title_rect.h / 2,
                    title_rect.w,
                    align,
                    label,
                );
            }
            for (context, rect) in &layout.buttons {
                self.draw_button(&mut canvas, look, *context, draw_rect(*rect), appearance);
            }
        }
        Some(image_of(pixmap, scale))
    }

    fn draw_button(
        &mut self,
        canvas: &mut Canvas<'_>,
        look: &Look,
        context: Context,
        rect: DrawRect,
        appearance: &Appearance<'_>,
    ) {
        let palette = &look.palette;
        let hovered = appearance.hover == Some(context);
        let pressed = appearance.pressed == Some(context);
        let glyph = if appearance.active {
            palette.button_active
        } else {
            palette.button_inactive
        };
        match look.style.glyphs {
            Glyphs::Circles => {
                let which = match context {
                    Context::Close => 0,
                    Context::Iconify => 1,
                    _ => 2,
                };
                let color = if appearance.active || hovered {
                    crate::theme::Palette::circle(which)
                } else {
                    palette.button_inactive
                };
                let radius = rect.w as f32 / 2.0;
                canvas.fill_rounded(rect, radius, color);
                if hovered {
                    let inset = DrawRect::new(rect.x + 3, rect.y + 3, rect.w - 6, rect.h - 6);
                    self.draw_glyph(
                        canvas,
                        context,
                        inset,
                        selkies_draw::style::on_color(color),
                        appearance,
                        false,
                    );
                }
            }
            Glyphs::Lines | Glyphs::Wide => {
                let close = context == Context::Close;
                let lit = hovered || pressed;
                if lit {
                    let fill = match (close, pressed) {
                        (true, false) => palette.close,
                        (true, true) => shifted(palette.close, -20),
                        (false, false) => with_alpha(glyph, 0x30),
                        (false, true) => with_alpha(glyph, 0x50),
                    };
                    let diameter = rect.w.min(rect.h) as f32 - 2.0;
                    let center_x = rect.x as f32 + rect.w as f32 / 2.0;
                    let center_y = rect.y as f32 + rect.h as f32 / 2.0;
                    if let Some(circle) = PathBuilder::from_circle(center_x, center_y, diameter / 2.0) {
                        canvas.fill_path(&circle, fill);
                    }
                }
                let side = rect.h.min(rect.w).min(12);
                let inset = DrawRect::new(
                    rect.x + (rect.w - side) / 2,
                    rect.y + (rect.h - side) / 2,
                    side,
                    side,
                );
                let color = if lit && close { Color::WHITE } else { glyph };
                let chevrons = look.style.glyphs == Glyphs::Lines;
                self.draw_glyph(canvas, context, inset, color, appearance, chevrons);
            }
        }
    }

    /// The line glyph of a button inside `rect`: a cross to close; with `chevrons`, a chevron
    /// pointing down to minimize, one pointing up to maximize, and a diamond to restore a
    /// maximized window, else a square (two for a maximized window) and a line.
    fn draw_glyph(
        &mut self,
        canvas: &mut Canvas<'_>,
        context: Context,
        rect: DrawRect,
        color: Color,
        appearance: &Appearance<'_>,
        chevrons: bool,
    ) {
        let x = rect.x as f32;
        let y = rect.y as f32;
        let w = rect.w as f32;
        let h = rect.h as f32;
        let mut path = PathBuilder::new();
        let (cx, cy) = (x + w / 2.0, y + h / 2.0);
        let reach = w / 2.0 - 1.0;
        match context {
            Context::Close => {
                path.move_to(x + 1.0, y + 1.0);
                path.line_to(x + w - 1.0, y + h - 1.0);
                path.move_to(x + w - 1.0, y + 1.0);
                path.line_to(x + 1.0, y + h - 1.0);
            }
            Context::Maximize if chevrons && appearance.maximized => {
                path.move_to(cx, cy - reach);
                path.line_to(cx + reach, cy);
                path.line_to(cx, cy + reach);
                path.line_to(cx - reach, cy);
                path.close();
            }
            Context::Maximize if chevrons => {
                path.move_to(cx - reach, cy + reach / 2.0);
                path.line_to(cx, cy - reach / 2.0);
                path.line_to(cx + reach, cy + reach / 2.0);
            }
            Context::Iconify if chevrons => {
                path.move_to(cx - reach, cy - reach / 2.0);
                path.line_to(cx, cy + reach / 2.0);
                path.line_to(cx + reach, cy - reach / 2.0);
            }
            Context::Maximize => {
                if appearance.maximized {
                    let inset = 2.5;
                    path.push_rect(
                        selkies_draw::tiny_skia::Rect::from_xywh(
                            x + 1.0,
                            y + 1.0 + inset,
                            w - 1.0 - inset,
                            h - 1.0 - inset,
                        )
                        .unwrap_or_else(|| {
                            selkies_draw::tiny_skia::Rect::from_xywh(0.0, 0.0, 1.0, 1.0).unwrap()
                        }),
                    );
                    path.move_to(x + 1.0 + inset, y + 1.0 + inset);
                    path.line_to(x + 1.0 + inset, y + 1.0);
                    path.line_to(x + w - 1.0, y + 1.0);
                    path.line_to(x + w - 1.0, y + h - 1.0 - inset);
                    path.line_to(x + w - 1.0 - inset, y + h - 1.0 - inset);
                } else {
                    path.push_rect(
                        selkies_draw::tiny_skia::Rect::from_xywh(x + 1.0, y + 1.0, w - 2.0, h - 2.0)
                            .unwrap_or_else(|| {
                                selkies_draw::tiny_skia::Rect::from_xywh(0.0, 0.0, 1.0, 1.0).unwrap()
                            }),
                    );
                }
            }
            _ => {
                path.move_to(x + 1.0, y + h - 1.5);
                path.line_to(x + w - 1.0, y + h - 1.5);
            }
        }
        if let Some(path) = path.finish() {
            canvas.stroke_path(&path, 1.5, color);
        }
    }

    /// The icon a frame shows: the window's own, else the theme's by app id.
    fn icon_pixmap(&mut self, appearance: &Appearance<'_>, size: u32) -> Option<Arc<Pixmap>> {
        if let Some(icon) = appearance.icon
            && let Some(pixmap) = pixmap_of(icon)
        {
            return Some(Arc::new(pixmap));
        }
        let name = appearance.app_id;
        if name.is_empty() {
            return None;
        }
        self.icons
            .get(name, size)
            .or_else(|| self.icons.get(&name.to_ascii_lowercase(), size))
            .or_else(|| {
                let short = name.rsplit('.').next().unwrap_or(name).to_ascii_lowercase();
                self.icons.get(&short, size)
            })
    }
}

/// A left or right border of `w` by `h` logical pixels: the title bar's base color, with the
/// frame's outline down the column at `edge`.
fn render_side(w: i32, h: i32, scale: i32, base: Color, border: Color, edge: i32) -> Image {
    let (pw, ph) = ((w.max(1) * scale) as u32, (h.max(1) * scale) as u32);
    let mut pixmap = Pixmap::new(pw, ph).expect("a border fits in memory");
    pixmap.fill(base);
    Canvas::new(pixmap.as_mut(), scale).fill_rect(DrawRect::new(edge, 0, 1, h.max(1)), border);
    image_of(pixmap, scale)
}

/// The bottom border in the title bar's base color, its outline along the bottom row and down
/// both outer columns, over the shadow when `under` names one and the part's place relative to
/// the client area.
///
/// The slab is a rounded rectangle reaching above the part, so only its bottom corners land in
/// the pixmap. Their radius is held to the border's height: a larger curve would start inside
/// the side borders, whose outline runs straight to the bottom.
fn render_bottom(
    w: i32,
    h: i32,
    scale: i32,
    base: Color,
    border: Color,
    radius: f32,
    under: Option<(&Shadow, i32, i32)>,
) -> Image {
    let Some(mut pixmap) = Pixmap::new((w * scale) as u32, (h * scale) as u32) else {
        return solid(w, h, scale, base);
    };
    {
        let mut canvas = Canvas::new(pixmap.as_mut(), scale);
        if let Some((shadow, x, y)) = under {
            shadow.paint(&mut canvas, x, y);
        }
        let above = radius.ceil() as i32 + 2;
        let slab = DrawRect::new(0, -above, w, h + above);
        let radius = radius.min(h as f32);
        canvas.fill_rounded(slab, radius, base);
        canvas.stroke_rounded(slab, radius, 1.0, border);
    }
    image_of(pixmap, scale)
}

/// The shadow a frame casts: one definition every part that shows it paints from.
///
/// The shadow is cast by the frame's rectangle moved [`SHADOW_OFFSET`] down and drawn as that
/// rectangle inflated by every second pixel up to [`SHADOW`], its corner radius grown by the
/// same amount, each layer at the opacity [`shadow_layers`] gives it. A part paints it in its own
/// coordinates and its own pixmap clips the rest, so no raster spans the window.
struct Shadow {
    /// The casting rectangle, relative to the client area.
    caster: Rect,
    /// The frame's own rectangle, relative to the client area.
    frame: Rect,
    /// The style's corner radius, the innermost layer's.
    radius: f32,
    /// How far each layer grows the caster and the opacity it is drawn at.
    layers: Vec<(i32, f32)>,
}

impl Shadow {
    /// The shadow of a frame at `frame` (relative to the client area), when the appearance asks
    /// for one and the window is not maximized.
    fn cast(appearance: &Appearance<'_>, style: &WindowStyle, frame: Rect) -> Option<Shadow> {
        if !appearance.shadow || appearance.maximized {
            return None;
        }
        let peak = if appearance.active {
            SHADOW_ACTIVE
        } else {
            SHADOW_INACTIVE
        };
        Some(Shadow {
            caster: Rect::new(frame.x, frame.y + SHADOW_OFFSET, frame.w, frame.h),
            frame,
            radius: style.corners as f32,
            layers: shadow_layers(peak),
        })
    }

    /// Paint the shadow into a canvas whose top-left corner sits at `x`, `y` relative to the
    /// client area.
    fn paint(&self, canvas: &mut Canvas<'_>, x: i32, y: i32) {
        for &(grow, alpha) in &self.layers {
            let layer = DrawRect::new(
                self.caster.x - x - grow,
                self.caster.y - y - grow,
                self.caster.w + 2 * grow,
                self.caster.h + 2 * grow,
            );
            let color = Color::from_rgba(0.0, 0.0, 0.0, alpha).unwrap_or(Color::TRANSPARENT);
            canvas.fill_rounded(layer, self.radius + grow as f32, color);
        }
    }
}

/// The strips that carry a shadow outside its frame: the top and bottom strips span the whole
/// band, corners included, and the side strips the frame's height between them, so the band is
/// [`SHADOW`] minus the offset above the frame and [`SHADOW`] plus it below. A strip that comes
/// out empty, or whose pixmap cannot be had, is left out.
fn render_shadow(shadow: &Shadow, scale: i32) -> Vec<FramePart> {
    let (frame, caster) = (shadow.frame, shadow.caster);
    let left = caster.x - SHADOW;
    let top = caster.y - SHADOW;
    let right = caster.right() + SHADOW;
    let bottom = caster.bottom() + SHADOW;
    let strips = [
        Rect::new(left, top, right - left, frame.y - top),
        Rect::new(left, frame.bottom(), right - left, bottom - frame.bottom()),
        Rect::new(left, frame.y, frame.x - left, frame.h),
        Rect::new(frame.right(), frame.y, right - frame.right(), frame.h),
    ];
    strips
        .into_iter()
        .filter(|strip| strip.w > 0 && strip.h > 0)
        .filter_map(|strip| {
            let mut pixmap = Pixmap::new((strip.w * scale) as u32, (strip.h * scale) as u32)?;
            shadow.paint(&mut Canvas::new(pixmap.as_mut(), scale), strip.x, strip.y);
            Some(FramePart {
                rect: strip,
                image: image_of(pixmap, scale),
            })
        })
        .collect()
}

/// The shadow's layers: how far each grows its rectangle and the opacity it is drawn at.
///
/// A point `d` pixels outside the casting rectangle lies inside every layer grown by `d` or more,
/// and black layers over one another reach `1 - Π(1 - a)`. Each layer's opacity is chosen so
/// that product telescopes to the falloff `peak · (1 - d / SHADOW)²` at every step, which a
/// plain constant per layer would only approximate; the outermost layer, at no opacity, is
/// dropped.
fn shadow_layers(peak: f32) -> Vec<(i32, f32)> {
    let falloff = |grow: i32| {
        let rest = 1.0 - (grow as f32 / SHADOW as f32).min(1.0);
        peak * rest * rest
    };
    (0..=SHADOW)
        .step_by(2)
        .map(|grow| (grow, 1.0 - (1.0 - falloff(grow)) / (1.0 - falloff(grow + 2))))
        .filter(|&(_, alpha)| alpha > 0.0)
        .collect()
}

/// Text shaped to fit a width, its tail an ellipsis when it does not.
pub fn shape_fit(fonts: &mut Fonts, text: &str, px: f32, max_width: f32) -> Arc<selkies_draw::render::Text> {
    let full = fonts.shape(text, px, false);
    if full.width <= max_width || max_width <= 0.0 {
        return full;
    }
    let chars: Vec<char> = text.chars().collect();
    let mut lo = 0usize;
    let mut hi = chars.len();
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let candidate: String = chars[..mid].iter().collect::<String>().trim_end().to_string() + "…";
        if fonts.shape(&candidate, px, false).width <= max_width {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let candidate: String = chars[..lo].iter().collect::<String>().trim_end().to_string() + "…";
    fonts.shape(&candidate, px, false)
}

/// A pixmap as the image a backend presents.
pub fn image_of(pixmap: Pixmap, scale: i32) -> Image {
    Image {
        width: pixmap.width(),
        height: pixmap.height(),
        scale,
        rgba: Arc::from(pixmap.data()),
    }
}

/// An image back as a pixmap, for drawing it into another.
pub fn pixmap_of(image: &Image) -> Option<Pixmap> {
    Pixmap::from_vec(
        image.rgba.to_vec(),
        selkies_draw::tiny_skia::IntSize::from_wh(image.width, image.height)?,
    )
}

/// A rectangle of one color.
pub fn solid(w: i32, h: i32, scale: i32, color: Color) -> Image {
    let (pw, ph) = ((w.max(1) * scale) as u32, (h.max(1) * scale) as u32);
    let mut pixmap = Pixmap::new(pw, ph).expect("a border fits in memory");
    pixmap.fill(color);
    image_of(pixmap, scale)
}

/// A layout rectangle as the drawing library's.
pub fn draw_rect(rect: Rect) -> DrawRect {
    DrawRect::new(rect.x, rect.y, rect.w, rect.h)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hit test names the title bar, each button, the borders, and the corners, and nothing
    /// inside the client.
    #[test]
    fn the_hit_test_names_every_part() {
        let style = WindowStyle::default();
        let layout = layout(&style, Size { w: 400, h: 300 }, false, false, Buttons::ALL);
        assert_eq!(layout.size, Size { w: 408, h: 334 });
        assert_eq!(hit(&layout, 200, 15, false), Some(Context::Titlebar));
        assert_eq!(hit(&layout, 200, 1, false), Some(Context::Top));
        assert_eq!(hit(&layout, 2, 1, false), Some(Context::TLCorner));
        assert_eq!(hit(&layout, 12, 15, false), Some(Context::Icon));
        let (close, close_rect) = layout.buttons.last().copied().unwrap();
        assert_eq!(close, Context::Close);
        assert_eq!(
            hit(&layout, close_rect.x + 2, close_rect.y + 2, false),
            Some(Context::Close)
        );
        assert_eq!(hit(&layout, 200, 200, false), None);
        assert_eq!(hit(&layout, 1, 200, false), Some(Context::Left));
        assert_eq!(hit(&layout, 405, 200, false), Some(Context::Right));
        assert_eq!(hit(&layout, 200, 332, false), Some(Context::Bottom));
        assert_eq!(hit(&layout, 405, 332, false), Some(Context::BRCorner));
        assert_eq!(hit(&layout, 1, 320, false), Some(Context::BLCorner));
        assert_eq!(hit(&layout, 500, 15, false), None);
        let maximized = super::layout(&style, Size { w: 400, h: 300 }, true, false, Buttons::ALL);
        assert_eq!(maximized.extents.left, 0);
        assert_eq!(hit(&maximized, 0, 200, false), None);
    }

    /// The buttons sit on the side the style names, in the order each side keeps.
    #[test]
    fn the_buttons_follow_the_style() {
        let right = layout(
            &WindowStyle::default(),
            Size { w: 400, h: 300 },
            false,
            false,
            Buttons::ALL,
        );
        let order: Vec<Context> = right.buttons.iter().map(|(c, _)| *c).collect();
        assert_eq!(order, vec![Context::Iconify, Context::Maximize, Context::Close]);
        assert!(right.buttons[2].1.x > right.buttons[0].1.x);
        let left = layout(
            &WindowStyle {
                buttons: ButtonSide::Left,
                glyphs: Glyphs::Circles,
                ..WindowStyle::default()
            },
            Size { w: 400, h: 300 },
            false,
            false,
            Buttons::ALL,
        );
        let order: Vec<Context> = left.buttons.iter().map(|(c, _)| *c).collect();
        assert_eq!(order, vec![Context::Close, Context::Iconify, Context::Maximize]);
        assert_eq!(left.buttons[0].1.x, PADDING);
    }

    /// The buttons the image hides leave the title bar, the ones left keeping their order at
    /// the edge, and the title takes the room.
    #[test]
    fn hidden_buttons_leave_the_title_bar() {
        let style = WindowStyle::default();
        let all = layout(&style, Size { w: 400, h: 300 }, false, false, Buttons::ALL);
        let close_only = layout(
            &style,
            Size { w: 400, h: 300 },
            false,
            false,
            Buttons::parse("close"),
        );
        let order: Vec<Context> = close_only.buttons.iter().map(|(c, _)| *c).collect();
        assert_eq!(order, vec![Context::Close]);
        assert_eq!(close_only.buttons[0].1, all.buttons[2].1);
        assert!(close_only.title.w > all.title.w);
        let none = layout(
            &style,
            Size { w: 400, h: 300 },
            false,
            false,
            Buttons::parse("none"),
        );
        assert!(none.buttons.is_empty());
        assert_eq!(Buttons::parse("minimize, maximize,close"), Buttons::ALL);
    }

    /// What a focused, unmaximized window without a shadow shows at an output scale.
    fn appearance(scale: i32, shadow: bool) -> Appearance<'static> {
        Appearance {
            title: "A window",
            icon: None,
            app_id: "",
            active: true,
            maximized: false,
            shaded: false,
            hover: None,
            pressed: None,
            scale,
            shadow,
            buttons: Buttons::ALL,
        }
    }

    /// The physical pixel of an image at a column and row, as its premultiplied bytes.
    fn pixel(image: &Image, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * image.width + x) * 4) as usize;
        [
            image.rgba[i],
            image.rgba[i + 1],
            image.rgba[i + 2],
            image.rgba[i + 3],
        ]
    }

    /// The bytes an opaque color is stored as.
    fn bytes(color: Color) -> [u8; 4] {
        let c = color.to_color_u8();
        [c.red(), c.green(), c.blue(), c.alpha()]
    }

    /// Whether two rectangles share any pixel.
    fn overlaps(a: Rect, b: Rect) -> bool {
        a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom()
    }

    /// A rendered frame has a title bar as wide as the frame and a border of the style's
    /// thickness, drawn at the output's scale.
    #[test]
    fn a_frame_renders_its_parts_at_scale() {
        let mut renderer = Renderer::new(None);
        let look = Look::default();
        let frame = renderer.render(&look, Size { w: 200, h: 100 }, &appearance(2, false));
        assert_eq!(frame.extents.top, TITLE_HEIGHT);
        assert_eq!(frame.parts.len(), 4);
        let top = &frame.parts[0];
        assert_eq!(top.rect, Rect::new(-BORDER, -TITLE_HEIGHT, 208, TITLE_HEIGHT));
        assert_eq!((top.image.width, top.image.height), (416, 60));
        assert_eq!(top.image.rgba.len(), 416 * 60 * 4);
        assert!(
            frame
                .parts
                .iter()
                .any(|p| p.rect == Rect::new(200, 0, BORDER, 100))
        );
    }

    /// The side borders are the title bar's color with the border color only along the frame's
    /// outer column.
    #[test]
    fn the_border_is_one_slab_with_an_outline() {
        let mut renderer = Renderer::new(None);
        let look = Look::default();
        let frame = renderer.render(&look, Size { w: 200, h: 100 }, &appearance(1, false));
        let left = frame
            .parts
            .iter()
            .find(|p| p.rect == Rect::new(-BORDER, 0, BORDER, 100))
            .unwrap();
        let right = frame
            .parts
            .iter()
            .find(|p| p.rect == Rect::new(200, 0, BORDER, 100))
            .unwrap();
        let (base, border) = (
            bytes(look.palette.title_active),
            bytes(look.palette.border_active),
        );
        assert_eq!(pixel(&left.image, 0, 50), border);
        assert_eq!(pixel(&left.image, 1, 50), base);
        assert_eq!(pixel(&right.image, BORDER as u32 - 1, 50), border);
        assert_eq!(pixel(&right.image, BORDER as u32 - 2, 50), base);
    }

    /// A shadow adds four strips around the frame, all outside its rectangle and deeper below it
    /// than above, shows under the corners the title bar and bottom border leave uncovered, and
    /// leaves the extents as they were.
    #[test]
    fn a_shadow_adds_four_strips_outside_the_frame() {
        let mut renderer = Renderer::new(None);
        let look = Look::default();
        let client = Size { w: 200, h: 100 };
        let plain = renderer.render(&look, client, &appearance(2, false));
        let shadowed = renderer.render(&look, client, &appearance(2, true));
        assert_eq!(shadowed.extents, plain.extents);
        for (plain, shadowed) in plain.parts.iter().zip(&shadowed.parts) {
            assert_eq!(plain.rect, shadowed.rect);
        }
        let corner = |frame: &Frame, part: usize, y: u32| pixel(&frame.parts[part].image, 0, y)[3];
        let bottom_part = plain.parts.len() - 1;
        let last_row = plain.parts[bottom_part].image.height - 1;
        assert_eq!(corner(&plain, 0, 0), 0);
        assert_eq!(corner(&plain, bottom_part, last_row), 0);
        assert!(corner(&shadowed, 0, 0) > 0);
        assert!(corner(&shadowed, bottom_part, last_row) > 0);
        let strips = &shadowed.parts[plain.parts.len()..];
        assert_eq!(strips.len(), 4);
        let frame = Rect::new(-BORDER, -TITLE_HEIGHT, 208, 134);
        for strip in strips {
            assert!(
                !overlaps(strip.rect, frame),
                "{:?} overlaps the frame",
                strip.rect
            );
            assert_eq!(strip.image.width, strip.rect.w as u32 * 2);
            assert_eq!(strip.image.height, strip.rect.h as u32 * 2);
        }
        let top = SHADOW - SHADOW_OFFSET;
        let bottom = SHADOW + SHADOW_OFFSET;
        assert_eq!(
            strips[0].rect,
            Rect::new(-BORDER - SHADOW, -TITLE_HEIGHT - top, 208 + 2 * SHADOW, top)
        );
        assert_eq!(
            strips[1].rect,
            Rect::new(-BORDER - SHADOW, 104, 208 + 2 * SHADOW, bottom)
        );
        let middle = 2 * (SHADOW + 100) as u32;
        let under = pixel(&strips[1].image, middle, 0)[3];
        let edge = pixel(&strips[1].image, middle, 2 * bottom as u32 - 1)[3];
        assert!(
            under > 0 && under > edge,
            "{under} at the frame, {edge} at the band's edge"
        );
        let maximized = Appearance {
            maximized: true,
            ..appearance(2, true)
        };
        assert_eq!(renderer.render(&look, client, &maximized).parts.len(), 1);
    }

    /// A hovered close button is a red circle: its center takes the fill and its bounding box's
    /// corner stays the title bar's.
    #[test]
    fn a_hovered_close_button_is_a_red_circle() {
        let mut renderer = Renderer::new(None);
        let look = Look::default();
        let hovered = Appearance {
            hover: Some(Context::Close),
            ..appearance(1, false)
        };
        let frame = renderer.render(&look, Size { w: 200, h: 100 }, &hovered);
        let layout = layout(&look.style, Size { w: 200, h: 100 }, false, false, Buttons::ALL);
        let (_, close) = layout.buttons.last().copied().unwrap();
        let title = &frame.parts[0].image;
        let center = pixel(title, (close.x + 2) as u32, (close.y + close.h / 2) as u32);
        assert_eq!(center, bytes(look.palette.close));
        assert_eq!(
            pixel(title, close.x as u32, close.y as u32),
            bytes(look.palette.title_active)
        );
    }
}
