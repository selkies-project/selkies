//! The display server backends.  Wayland is tried first because a labwc
//! session also exports `DISPLAY` for XWayland, where the X11 backend would
//! have no window manager to cooperate with; X11 is used when no compositor
//! with layer shell support can be reached.

pub mod wayland;
pub mod x11;

use std::os::fd::RawFd;

use crate::desktop::Frame;
use crate::event::{Event, Request, SurfaceId};

/// What a display server backend provides to the main loop.
pub trait Backend {
    /// A short name for log messages.
    fn name(&self) -> &'static str;

    /// The connection descriptor to poll for readability.
    fn fd(&self) -> RawFd;

    /// Handles everything already queued and flushes requests, so the main
    /// loop can safely block in poll.
    ///
    /// # Errors
    /// The connection failed.
    fn prepare(&mut self) -> Result<(), String>;

    /// Whether events gathered while `prepare` ran, or while a reply was
    /// awaited, are waiting to be dispatched, so the main loop must not block
    /// in poll: the connection descriptor stays quiet for data already read.
    fn pending(&self) -> bool;

    /// Reads and handles new events after the poll woke up.
    ///
    /// # Arguments
    /// * `readable` - Whether the connection descriptor was readable.
    ///
    /// # Errors
    /// The connection failed.
    fn dispatch(&mut self, readable: bool) -> Result<Vec<Event>, String>;

    /// Shows a rendered frame on a surface.
    ///
    /// # Arguments
    /// * `surface` - The surface the frame belongs to.
    /// * `frame` - The pixels and the regions that changed.
    fn present(&mut self, surface: SurfaceId, frame: &Frame<'_>);

    /// Whether the desktop's launchers go on a layer of their own above the
    /// wallpaper, so that another program's wallpaper layer stays beneath
    /// them instead of covering them.
    ///
    /// # Returns
    /// `true` where `present_launchers` shows the launchers; otherwise a
    /// background's frame carries them.
    fn layered_desktop(&self) -> bool {
        false
    }

    /// Shows a background's launchers on their own layer, where the backend
    /// keeps one (`layered_desktop`); input on that layer reaches the desktop
    /// as the background's.
    ///
    /// # Arguments
    /// * `background` - The background the launchers belong to.
    /// * `frame` - The launchers over a transparent canvas, taking input on
    ///   their cells.
    fn present_launchers(&mut self, _background: u32, _frame: &Frame<'_>) {}

    /// Carries out a request from the desktop.
    ///
    /// # Arguments
    /// * `request` - The request.
    fn perform(&mut self, request: &Request);

    /// Where the panel surface's top-left corner sits in the X screen the
    /// tray icons live on, in that screen's pixels, with the scale that turns
    /// the panel's logical coordinates into them: the output scale on X11,
    /// where the screen is physical pixels, and one under XWayland, whose
    /// screen is the compositor's logical layout.
    fn panel_origin(&self) -> (i32, i32, i32);
}

/// Connects to the first display server that works.
///
/// # Returns
/// The backend, or `None` when neither a layer shell compositor nor an X
/// server is reachable.
pub fn connect() -> Option<Box<dyn Backend>> {
    match wayland::WaylandBackend::connect() {
        Ok(backend) => return Some(Box::new(backend)),
        Err(reason) => {
            if std::env::var_os("SELKIES_DESKTOP_DEBUG").is_some() {
                eprintln!("selkies-desktop: wayland unavailable: {reason}");
            }
        }
    }
    match x11::X11Backend::connect() {
        Ok(backend) => Some(Box::new(backend)),
        Err(reason) => {
            if std::env::var_os("SELKIES_DESKTOP_DEBUG").is_some() {
                eprintln!("selkies-desktop: x11 unavailable: {reason}");
            }
            None
        }
    }
}

/// Copies the premultiplied RGBA pixels of a frame rectangle into a BGRA
/// buffer of the same dimensions, which is what both display servers want on
/// little-endian hosts.
///
/// # Arguments
/// * `src` - Source pixels, RGBA, `stride` bytes per row.
/// * `dst` - Destination pixels, BGRA, same stride and dimensions.
/// * `stride` - Bytes per row.
/// * `x` - Left edge of the rectangle in pixels.
/// * `y` - Top edge of the rectangle in pixels.
/// * `w` - Width in pixels.
/// * `h` - Height in pixels.
pub fn copy_bgra(src: &[u8], dst: &mut [u8], stride: usize, x: usize, y: usize, w: usize, h: usize) {
    for row in y..y + h {
        let start = row * stride + x * 4;
        let end = start + w * 4;
        if end > src.len() || end > dst.len() {
            break;
        }
        for (d, s) in dst[start..end]
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(src[start..end].as_chunks::<4>().0)
        {
            d[0] = s[2];
            d[1] = s[1];
            d[2] = s[0];
            d[3] = s[3];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::copy_bgra;

    #[test]
    fn swizzles_only_the_rectangle() {
        let src: Vec<u8> = (0..2 * 2 * 4).map(|i| i as u8).collect();
        let mut dst = vec![0u8; src.len()];
        copy_bgra(&src, &mut dst, 8, 1, 1, 1, 1);
        assert_eq!(&dst[..12], &[0u8; 12]);
        assert_eq!(&dst[12..16], &[14, 13, 12, 15]);
    }
}
