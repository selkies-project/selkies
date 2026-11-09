/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The X windows the manager draws into: a client's frame, which the client is reparented into,
//! and the override-redirect windows an overlay (a menu, the Alt-Tab list) is shown in. Each is
//! painted by the server from a pixmap the engine's image is put into, so an expose costs the
//! manager nothing; the frame's rounded corners are cut with the Shape extension from the image's
//! alpha, since the server composites nothing. A client that shapes itself (a panel whose window
//! covers the room its popups open in, a dialog with rounded corners) has its bounding and input
//! shapes carried onto the frame, moved by the extents, so the frame hides and takes the pointer
//! exactly where the client does; without that the frame's rectangle would cover what the
//! client leaves showing.
//!
//! Everything here is in the server's pixels: the manager converts the engine's logical
//! rectangles with the session's scale before it calls in, and an image the engine drew at that
//! scale is put down pixel for pixel, resampled only where the two differ.

use pixelflux::wm::{Extents, Frame, Image, Rect};
use x11rb::connection::Connection;
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    self, ChangeWindowAttributesAux, ConfigureWindowAux, ConnectionExt, CreateGCAux, CreateWindowAux,
    EventMask, ImageFormat, Pixmap, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;

/// The events a frame window reports.
pub fn frame_events() -> EventMask {
    EventMask::BUTTON_PRESS
        | EventMask::BUTTON_RELEASE
        | EventMask::POINTER_MOTION
        | EventMask::ENTER_WINDOW
        | EventMask::LEAVE_WINDOW
        | EventMask::SUBSTRUCTURE_REDIRECT
        | EventMask::SUBSTRUCTURE_NOTIFY
        | EventMask::EXPOSURE
}

/// A frame window around one client.
pub struct FrameWindow {
    pub window: Window,
    pub extents: Extents,
    /// The frame's geometry on the root.
    pub rect: Rect,
    /// The client area's geometry on the root, what the engine configured.
    pub client: Rect,
    pixmap: Option<Pixmap>,
    gc: xproto::Gcontext,
    /// Whether the server is told to paint the title's color alone, during an effect.
    plain: bool,
    /// The opaque rectangles of the decoration last drawn, in frame coordinates.
    decor_shape: Vec<xproto::Rectangle>,
    /// The client's own bounding shape in client coordinates, where it set one.
    client_bounding: Option<Vec<xproto::Rectangle>>,
    /// The client's own input shape in client coordinates, where it set one.
    client_input: Option<Vec<xproto::Rectangle>>,
}

/// The depth and visual the frames and overlays are drawn in: the root's.
pub struct Visual {
    pub depth: u8,
    pub visual: xproto::Visualid,
    pub root: Window,
}

impl FrameWindow {
    /// Create a frame around `client` with the engine's `extents`, not yet mapped; the client
    /// is reparented into it by the caller.
    pub fn create(
        conn: &RustConnection,
        visual: &Visual,
        client: Rect,
        extents: Extents,
        background: u32,
    ) -> Result<FrameWindow, String> {
        let window = conn.generate_id().map_err(|e| e.to_string())?;
        let rect = client.grown(extents);
        conn.create_window(
            visual.depth,
            window,
            visual.root,
            rect.x as i16,
            rect.y as i16,
            rect.w.max(1) as u16,
            rect.h.max(1) as u16,
            0,
            WindowClass::INPUT_OUTPUT,
            visual.visual,
            &CreateWindowAux::new()
                .event_mask(frame_events())
                .background_pixel(background)
                .override_redirect(0)
                .bit_gravity(xproto::Gravity::NORTH_WEST),
        )
        .map_err(|e| e.to_string())?;
        let gc = conn.generate_id().map_err(|e| e.to_string())?;
        conn.create_gc(gc, window, &CreateGCAux::new().graphics_exposures(0))
            .map_err(|e| e.to_string())?;
        Ok(FrameWindow {
            window,
            extents,
            rect,
            client,
            pixmap: None,
            gc,
            plain: false,
            decor_shape: Vec::new(),
            client_bounding: None,
            client_input: None,
        })
    }

    /// Move and size the frame to hold `client` with `extents`, the client window inside it
    /// placed at the extents' corner.
    pub fn place(&mut self, conn: &RustConnection, client_window: Window, client: Rect, extents: Extents) {
        let resized = client.w != self.client.w || client.h != self.client.h || extents != self.extents;
        self.extents = extents;
        self.client = client;
        let rect = client.grown(extents);
        let moved = rect != self.rect;
        self.rect = rect;
        if moved {
            let _ = conn.configure_window(
                self.window,
                &ConfigureWindowAux::new()
                    .x(rect.x)
                    .y(rect.y)
                    .width(rect.w.max(1) as u32)
                    .height(rect.h.max(1) as u32),
            );
        }
        let _ = conn.configure_window(
            client_window,
            &ConfigureWindowAux::new()
                .x(extents.left)
                .y(extents.top)
                .width(client.w.max(1) as u32)
                .height(client.h.max(1) as u32)
                .border_width(0),
        );
        // The bounding shape is a rectangle list the server keeps as it was set, so a frame
        // that grew would stay clipped to its old client area until the next draw, which an
        // undecorated window never gets.
        if resized && !self.plain {
            self.apply_shape(conn);
        }
    }

    /// Take over the client's shapes: `bounding` and `input` are its rectangles in its own
    /// coordinates, `None` where it is not shaped that way, and the frame's shapes follow.
    pub fn set_client_shape(
        &mut self,
        conn: &RustConnection,
        bounding: Option<Vec<xproto::Rectangle>>,
        input: Option<Vec<xproto::Rectangle>>,
    ) {
        self.client_bounding = bounding;
        self.client_input = input;
        if !self.plain {
            self.apply_shape(conn);
        }
    }

    /// Set the frame's bounding and input shapes: the decoration's opaque parts, plus the
    /// client's shape of each kind moved by the extents, or the whole client area where it has
    /// none; an input shape is dropped where neither has one, so the bounding one serves.
    fn apply_shape(&self, conn: &RustConnection) {
        let client_rect = xproto::Rectangle {
            x: self.extents.left as i16,
            y: self.extents.top as i16,
            width: self.client.w.max(1) as u16,
            height: self.client.h.max(1) as u16,
        };
        let moved = |rects: &[xproto::Rectangle]| {
            rects
                .iter()
                .map(|r| xproto::Rectangle {
                    x: r.x.saturating_add(self.extents.left as i16),
                    y: r.y.saturating_add(self.extents.top as i16),
                    width: r.width,
                    height: r.height,
                })
                .collect::<Vec<_>>()
        };
        let mut bounding = self.decor_shape.clone();
        match &self.client_bounding {
            Some(rects) => bounding.extend(moved(rects)),
            None => bounding.push(client_rect),
        }
        let _ = conn.shape_rectangles(
            shape::SO::SET,
            shape::SK::BOUNDING,
            xproto::ClipOrdering::UNSORTED,
            self.window,
            0,
            0,
            &bounding,
        );
        match &self.client_input {
            Some(rects) => {
                let mut input = self.decor_shape.clone();
                input.extend(moved(rects));
                let _ = conn.shape_rectangles(
                    shape::SO::SET,
                    shape::SK::INPUT,
                    xproto::ClipOrdering::UNSORTED,
                    self.window,
                    0,
                    0,
                    &input,
                );
            }
            None => {
                let _ = conn.shape_mask(shape::SO::SET, shape::SK::INPUT, self.window, 0, 0, 0u32);
            }
        }
    }

    /// Show the frame as a plain rectangle of `color` at `rect` (an effect's step), the image
    /// left aside until `draw` paints it again.
    pub fn show_plain(&mut self, conn: &RustConnection, rect: Rect, color: u32) {
        if !self.plain {
            let _ = conn.change_window_attributes(
                self.window,
                &ChangeWindowAttributesAux::new()
                    .background_pixel(color)
                    .background_pixmap(0u32),
            );
            let _ = conn.shape_mask(shape::SO::SET, shape::SK::BOUNDING, self.window, 0, 0, 0u32);
            self.plain = true;
        }
        self.rect = rect;
        let _ = conn.configure_window(
            self.window,
            &ConfigureWindowAux::new()
                .x(rect.x)
                .y(rect.y)
                .width(rect.w.max(1) as u32)
                .height(rect.h.max(1) as u32),
        );
        let _ = conn.clear_area(false, self.window, 0, 0, 0, 0);
    }

    /// Paint the engine's frame into the window's background: every part at its place, scaled
    /// from the engine's logical coordinates by `scale`, the corners shaped from the parts' alpha.
    pub fn draw(&mut self, conn: &RustConnection, visual: &Visual, frame: &Frame, scale: i32) {
        let w = self.rect.w.max(1) as u16;
        let h = self.rect.h.max(1) as u16;
        if let Some(old) = self.pixmap.take() {
            let _ = conn.free_pixmap(old);
        }
        let Ok(pixmap) = conn.generate_id() else {
            return;
        };
        if conn
            .create_pixmap(visual.depth, pixmap, self.window, w, h)
            .is_err()
        {
            return;
        }
        let mut shaped = Vec::new();
        let scale = scale.max(1);
        for part in &frame.parts {
            let x = part.rect.x * scale + self.extents.left;
            let y = part.rect.y * scale + self.extents.top;
            let (w, h) = (part.rect.w * scale, part.rect.h * scale);
            put_image(conn, pixmap, self.gc, &part.image, Rect::new(x, y, w, h));
            shaped.extend(opaque_rectangles(&part.image, x, y, w, h));
        }
        let _ = conn.change_window_attributes(
            self.window,
            &ChangeWindowAttributesAux::new().background_pixmap(pixmap),
        );
        self.plain = false;
        self.pixmap = Some(pixmap);
        self.decor_shape = shaped;
        self.apply_shape(conn);
        let _ = conn.clear_area(true, self.window, 0, 0, 0, 0);
    }

    /// Free the frame's resources; the window itself is destroyed by the caller after
    /// reparenting the client away.
    pub fn destroy(&mut self, conn: &RustConnection) {
        if let Some(pixmap) = self.pixmap.take() {
            let _ = conn.free_pixmap(pixmap);
        }
        let _ = conn.free_gc(self.gc);
        let _ = conn.destroy_window(self.window);
    }
}

/// Put a premultiplied RGBA image into a drawable at a pixel rectangle, pixel for pixel where
/// the image has the rectangle's size and resampled to it where it does not.
fn put_image(
    conn: &RustConnection,
    drawable: xproto::Drawable,
    gc: xproto::Gcontext,
    image: &Image,
    at: Rect,
) {
    let (x, y, w, h) = (at.x, at.y, at.w, at.h);
    let (iw, ih) = (image.width, image.height);
    let (w, h) = (w.max(1) as u32, h.max(1) as u32);
    let mut data = Vec::with_capacity((w * h * 4) as usize);
    for row in 0..h {
        let sy = (row * ih / h).min(ih.saturating_sub(1));
        for col in 0..w {
            let sx = (col * iw / w).min(iw.saturating_sub(1));
            let o = ((sy * iw + sx) * 4) as usize;
            let (r, g, b, a) = (
                image.rgba.get(o).copied().unwrap_or(0),
                image.rgba.get(o + 1).copied().unwrap_or(0),
                image.rgba.get(o + 2).copied().unwrap_or(0),
                image.rgba.get(o + 3).copied().unwrap_or(0),
            );
            let un = |c: u8| {
                if a == 0 {
                    0
                } else {
                    ((c as u32 * 255) / a as u32).min(255) as u8
                }
            };
            data.extend_from_slice(&[un(b), un(g), un(r), 0]);
        }
    }
    let max_rows = (65536 * 3 / (w * 4)).max(1);
    let mut row = 0;
    while row < h {
        let rows = max_rows.min(h - row);
        let start = (row * w * 4) as usize;
        let end = ((row + rows) * w * 4) as usize;
        let _ = conn.put_image(
            ImageFormat::Z_PIXMAP,
            drawable,
            gc,
            w as u16,
            rows as u16,
            x as i16,
            (y + row as i32) as i16,
            0,
            24,
            &data[start..end],
        );
        row += rows;
    }
}

/// The runs of a part's pixels that are more than half opaque, as rectangles at its place of
/// `w` by `h` pixels, for the frame's shape.
fn opaque_rectangles(image: &Image, x: i32, y: i32, w: i32, h: i32) -> Vec<xproto::Rectangle> {
    let (iw, ih) = (image.width, image.height);
    let (w, h) = (w.max(1) as u32, h.max(1) as u32);
    let mut rects = Vec::new();
    for row in 0..h {
        let sy = (row * ih / h).min(ih.saturating_sub(1));
        let mut run: Option<u32> = None;
        for col in 0..=w {
            let opaque = col < w && {
                let sx = (col * iw / w).min(iw.saturating_sub(1));
                let o = ((sy * iw + sx) * 4 + 3) as usize;
                image.rgba.get(o).copied().unwrap_or(0) >= 128
            };
            match (run, opaque) {
                (None, true) => run = Some(col),
                (Some(start), false) => {
                    rects.push(xproto::Rectangle {
                        x: (x + start as i32) as i16,
                        y: (y + row as i32) as i16,
                        width: (col - start) as u16,
                        height: 1,
                    });
                    run = None;
                }
                _ => {}
            }
        }
    }
    merge_rows(rects)
}

/// Rows of identical runs merged into taller rectangles.
fn merge_rows(mut rects: Vec<xproto::Rectangle>) -> Vec<xproto::Rectangle> {
    let mut merged: Vec<xproto::Rectangle> = Vec::new();
    rects.sort_by_key(|r| (r.x, r.width, r.y));
    for r in rects {
        if let Some(last) = merged.last_mut()
            && last.x == r.x
            && last.width == r.width
            && last.y as i32 + last.height as i32 == r.y as i32
        {
            last.height += r.height;
        } else {
            merged.push(r);
        }
    }
    merged
}

/// An overlay window: override-redirect, above everything, painted from the engine's image.
pub struct OverlayWindow {
    pub window: Window,
    pub rect: Rect,
    pixmap: Option<Pixmap>,
    gc: xproto::Gcontext,
}

impl OverlayWindow {
    /// Create and show an overlay at `rect` with `image`.
    pub fn create(
        conn: &RustConnection,
        visual: &Visual,
        rect: Rect,
        image: &Image,
    ) -> Result<OverlayWindow, String> {
        let window = conn.generate_id().map_err(|e| e.to_string())?;
        conn.create_window(
            visual.depth,
            window,
            visual.root,
            rect.x as i16,
            rect.y as i16,
            rect.w.max(1) as u16,
            rect.h.max(1) as u16,
            0,
            WindowClass::INPUT_OUTPUT,
            visual.visual,
            &CreateWindowAux::new()
                .override_redirect(1)
                .save_under(1)
                .event_mask(
                    EventMask::BUTTON_PRESS
                        | EventMask::BUTTON_RELEASE
                        | EventMask::POINTER_MOTION
                        | EventMask::EXPOSURE,
                ),
        )
        .map_err(|e| e.to_string())?;
        let gc = conn.generate_id().map_err(|e| e.to_string())?;
        conn.create_gc(gc, window, &CreateGCAux::new().graphics_exposures(0))
            .map_err(|e| e.to_string())?;
        let mut overlay = OverlayWindow {
            window,
            rect,
            pixmap: None,
            gc,
        };
        overlay.update(conn, visual, rect, image);
        let _ = conn.map_window(window);
        let _ = conn.configure_window(
            window,
            &ConfigureWindowAux::new().stack_mode(xproto::StackMode::ABOVE),
        );
        Ok(overlay)
    }

    /// Move the overlay and paint a new image.
    pub fn update(&mut self, conn: &RustConnection, visual: &Visual, rect: Rect, image: &Image) {
        if rect != self.rect {
            self.rect = rect;
            let _ = conn.configure_window(
                self.window,
                &ConfigureWindowAux::new()
                    .x(rect.x)
                    .y(rect.y)
                    .width(rect.w.max(1) as u32)
                    .height(rect.h.max(1) as u32),
            );
        }
        if let Some(old) = self.pixmap.take() {
            let _ = conn.free_pixmap(old);
        }
        let Ok(pixmap) = conn.generate_id() else {
            return;
        };
        let w = rect.w.max(1) as u16;
        let h = rect.h.max(1) as u16;
        if conn
            .create_pixmap(visual.depth, pixmap, self.window, w, h)
            .is_err()
        {
            return;
        }
        put_image(conn, pixmap, self.gc, image, Rect::new(0, 0, rect.w, rect.h));
        let shaped = opaque_rectangles(image, 0, 0, rect.w, rect.h);
        let _ = conn.shape_rectangles(
            shape::SO::SET,
            shape::SK::BOUNDING,
            xproto::ClipOrdering::UNSORTED,
            self.window,
            0,
            0,
            &shaped,
        );
        let _ = conn.change_window_attributes(
            self.window,
            &ChangeWindowAttributesAux::new().background_pixmap(pixmap),
        );
        self.pixmap = Some(pixmap);
        let _ = conn.clear_area(true, self.window, 0, 0, 0, 0);
    }

    /// Take the overlay down.
    pub fn destroy(&mut self, conn: &RustConnection) {
        if let Some(pixmap) = self.pixmap.take() {
            let _ = conn.free_pixmap(pixmap);
        }
        let _ = conn.free_gc(self.gc);
        let _ = conn.destroy_window(self.window);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// The shape of an image is the runs of its opaque pixels, merged across rows.
    #[test]
    fn the_shape_follows_the_alpha() {
        let mut rgba = vec![0u8; 4 * 4 * 4];
        for y in 0..4 {
            for x in 1..3 {
                rgba[(y * 4 + x) * 4 + 3] = 255;
            }
        }
        let image = Image {
            width: 4,
            height: 4,
            scale: 1,
            rgba: Arc::from(rgba),
        };
        let rects = opaque_rectangles(&image, 10, 20, 4, 4);
        assert_eq!(rects.len(), 1);
        assert_eq!(
            (rects[0].x, rects[0].y, rects[0].width, rects[0].height),
            (11, 20, 2, 4)
        );
    }
}
