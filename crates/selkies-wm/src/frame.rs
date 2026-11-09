/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Server-side window frames: their layout around a client area, the hit test that names what a
//! point on them means, and their rasterization through `selkies-draw` into the parts a backend
//! presents.
//!
//! A frame is a title bar over the client and a border down its other three sides. The title bar
//! carries the window's icon, its title, and the iconify, maximize, and close buttons on the side
//! the desktop's window style names, drawn as line glyphs, as colored circles, or as wide flat
//! buttons; its top corners are rounded by the style's radius, as the shell's popups are, unless
//! the window is maximized. The border is the resize handle: its corners pull two edges, its
//! sides one, and the title bar's top rows pull the top edge, so a frame of a few pixels still
//! resizes from every side.
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
use selkies_draw::style::{ButtonSide, Glyphs, Surface, WindowStyle};
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

/// The layout of a frame around a client of `client` logical pixels.
pub fn layout(style: &WindowStyle, client: Size, maximized: bool, shaded: bool) -> Layout {
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
    let order: [Context; 3] = match style.buttons {
        ButtonSide::Right => [Context::Iconify, Context::Maximize, Context::Close],
        ButtonSide::Left => [Context::Close, Context::Iconify, Context::Maximize],
    };
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
            for context in order {
                buttons.push((context, Rect::new(x, top, side, button_h)));
                x += step;
            }
        }
    }
    let buttons_w = 3 * step + pad;
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

    /// The frame around a client of `client` logical pixels, every part drawn.
    pub fn render(&mut self, look: &Look, client: Size, appearance: &Appearance<'_>) -> Frame {
        let style = &look.style;
        let layout = layout(style, client, appearance.maximized, appearance.shaded);
        let e = layout.extents;
        let Size { w, h } = layout.size;
        let scale = appearance.scale.max(1);
        let mut parts = Vec::new();
        let origin_x = -e.left;
        let origin_y = -e.top;
        if let Some(image) = self.render_titlebar(look, &layout, appearance, scale) {
            parts.push(FramePart {
                rect: Rect::new(origin_x, origin_y, w, e.top),
                image,
            });
        }
        let border = if appearance.active {
            look.palette.border_active
        } else {
            look.palette.border_inactive
        };
        let radius = if appearance.maximized {
            0.0
        } else {
            style.corners as f32
        };
        let body_h = h - e.top;
        if e.left > 0 && body_h > 0 {
            parts.push(FramePart {
                rect: Rect::new(origin_x, 0, e.left, body_h - e.bottom),
                image: solid(e.left, body_h - e.bottom, scale, border),
            });
            parts.push(FramePart {
                rect: Rect::new(client.w, 0, e.right, body_h - e.bottom),
                image: solid(e.right, body_h - e.bottom, scale, border),
            });
        }
        if e.bottom > 0 {
            let image = self.render_bottom(w, e.bottom, scale, border, radius);
            parts.push(FramePart {
                rect: Rect::new(origin_x, h - e.top - e.bottom, w, e.bottom),
                image,
            });
        }
        Frame { extents: e, parts }
    }

    fn render_titlebar(
        &mut self,
        look: &Look,
        layout: &Layout,
        appearance: &Appearance<'_>,
        scale: i32,
    ) -> Option<Image> {
        let style = &look.style;
        let palette = &look.palette;
        let w = layout.size.w;
        let h = layout.extents.top;
        let mut pixmap = Pixmap::new((w * scale) as u32, (h * scale) as u32)?;
        {
            let mut canvas = Canvas::new(pixmap.as_mut(), scale);
            let active = appearance.active;
            let radius = if appearance.maximized {
                0.0
            } else {
                style.corners as f32
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
            canvas.fill_rect(DrawRect::new(0, h - 1, w, 1), border);
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
                    );
                }
            }
            Glyphs::Lines | Glyphs::Wide => {
                if pressed {
                    canvas.fill_rounded(rect, 4.0, selkies_draw::style::with_alpha(glyph, 0x30));
                } else if hovered {
                    let color = if context == Context::Close {
                        crate::theme::Palette::circle(0)
                    } else {
                        selkies_draw::style::with_alpha(glyph, 0x20)
                    };
                    canvas.fill_rounded(rect, 4.0, color);
                }
                let side = rect.h.min(rect.w).min(12);
                let inset = DrawRect::new(
                    rect.x + (rect.w - side) / 2,
                    rect.y + (rect.h - side) / 2,
                    side,
                    side,
                );
                let color = if hovered && context == Context::Close {
                    Color::WHITE
                } else {
                    glyph
                };
                self.draw_glyph(canvas, context, inset, color, appearance);
            }
        }
    }

    /// The line glyph of a button inside `rect`: a cross, a square (two for a maximized window),
    /// or a line.
    fn draw_glyph(
        &mut self,
        canvas: &mut Canvas<'_>,
        context: Context,
        rect: DrawRect,
        color: Color,
        appearance: &Appearance<'_>,
    ) {
        let x = rect.x as f32;
        let y = rect.y as f32;
        let w = rect.w as f32;
        let h = rect.h as f32;
        let mut path = PathBuilder::new();
        match context {
            Context::Close => {
                path.move_to(x + 1.0, y + 1.0);
                path.line_to(x + w - 1.0, y + h - 1.0);
                path.move_to(x + w - 1.0, y + 1.0);
                path.line_to(x + 1.0, y + h - 1.0);
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

    /// The bottom border with its rounded corners.
    fn render_bottom(&mut self, w: i32, h: i32, scale: i32, color: Color, radius: f32) -> Image {
        let Some(mut pixmap) = Pixmap::new((w * scale) as u32, (h * scale) as u32) else {
            return solid(w, h, scale, color);
        };
        {
            let mut canvas = Canvas::new(pixmap.as_mut(), scale);
            if radius > 0.0 {
                let extra = radius.ceil() as i32 + 2;
                canvas.clip(DrawRect::new(0, 0, w, h));
                canvas.fill_rounded(
                    DrawRect::new(0, -extra, w, h + extra),
                    radius.min(h as f32),
                    color,
                );
            } else {
                canvas.fill_rect(DrawRect::new(0, 0, w, h), color);
            }
        }
        image_of(pixmap, scale)
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
        let layout = layout(&style, Size { w: 400, h: 300 }, false, false);
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
        let maximized = super::layout(&style, Size { w: 400, h: 300 }, true, false);
        assert_eq!(maximized.extents.left, 0);
        assert_eq!(hit(&maximized, 0, 200, false), None);
    }

    /// The buttons sit on the side the style names, in the order each side keeps.
    #[test]
    fn the_buttons_follow_the_style() {
        let right = layout(&WindowStyle::default(), Size { w: 400, h: 300 }, false, false);
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
        );
        let order: Vec<Context> = left.buttons.iter().map(|(c, _)| *c).collect();
        assert_eq!(order, vec![Context::Close, Context::Iconify, Context::Maximize]);
        assert_eq!(left.buttons[0].1.x, PADDING);
    }

    /// A rendered frame has a title bar as wide as the frame and a border of the style's
    /// thickness, drawn at the output's scale.
    #[test]
    fn a_frame_renders_its_parts_at_scale() {
        let mut renderer = Renderer::new(None);
        let look = Look::default();
        let frame = renderer.render(
            &look,
            Size { w: 200, h: 100 },
            &Appearance {
                title: "A window",
                icon: None,
                app_id: "",
                active: true,
                maximized: false,
                shaded: false,
                hover: None,
                pressed: None,
                scale: 2,
            },
        );
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
}
