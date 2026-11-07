/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! What a capture hands its consumer, and the traits a consumer implements to receive it:
//! the encoded frames of a capture ([`FrameSink`]), the pointer's image as it changes
//! ([`CursorSink`]), the clipboard a client application copies to ([`ClipboardSink`]), and
//! the selection of an application compositor watched from outside ([`SelectionWatch`]).
//!
//! Every sink is called on a thread pixelflux owns and never on a capture or compositor
//! thread: a capture's deliver thread hands frames one batch at a time with one batch in
//! flight, so a consumer that blocks holds the encoder back by exactly one frame and loses
//! none; cursor images come from the `wl-cursor` worker or the X11 cursor monitor,
//! clipboard contents from a reader thread per copy. The Python bridge implements each
//! trait by taking the GIL and calling the registered callable; a Rust host implements it
//! directly, or hands a closure, which every trait here accepts.

pub use crate::encoders::reference::Reference;
pub use crate::encoders::software::{EncodedStripe as Frame, FrameTiming};

/// A consumer of the frames a capture encodes.
pub trait FrameSink: Send + Sync + 'static {
    /// Every stripe of one encoded frame, in capture order, each sharing the encoder's
    /// buffer.
    fn frames(&self, frames: Vec<Frame>);
}

impl<F: Fn(Vec<Frame>) + Send + Sync + 'static> FrameSink for F {
    fn frames(&self, frames: Vec<Frame>) {
        self(frames)
    }
}

/// The pointer's image as the compositor or the X server shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorImage {
    /// `png` for an image, `hide` for a pointer that shows nothing, `error` where the
    /// sprite could not be read.
    pub kind: &'static str,
    /// The sprite as a straight-alpha PNG, empty unless `kind` is `png`.
    pub png: Vec<u8>,
    /// The hotspot within the sprite, in its pixels.
    pub hot_x: i32,
    pub hot_y: i32,
}

/// A consumer of cursor images.
pub trait CursorSink: Send + Sync + 'static {
    fn cursor(&self, image: CursorImage);
}

impl<F: Fn(CursorImage) + Send + Sync + 'static> CursorSink for F {
    fn cursor(&self, image: CursorImage) {
        self(image)
    }
}

/// A consumer of what client applications copy to the compositor's clipboard.
pub trait ClipboardSink: Send + Sync + 'static {
    /// The flavors of one copy as `(mime, bytes)` entries: the picture, or the markup and
    /// the text beneath it, and a password manager's hint beside a secret. A client
    /// clearing the clipboard hands over no entries.
    fn selection(&self, entries: Vec<(String, Vec<u8>)>);
}

impl<F: Fn(Vec<(String, Vec<u8>)>) + Send + Sync + 'static> ClipboardSink for F {
    fn selection(&self, entries: Vec<(String, Vec<u8>)>) {
        self(entries)
    }
}

/// A consumer of the selection changes of an application compositor watched over its own
/// socket (`ScreenCapture::clipboard_watch_app`).
pub trait SelectionWatch: Send + Sync + 'static {
    /// The mime types the new selection offers, none for a cleared selection.
    fn mimes(&self, mimes: Vec<String>);
}

impl<F: Fn(Vec<String>) + Send + Sync + 'static> SelectionWatch for F {
    fn mimes(&self, mimes: Vec<String>) {
        self(mimes)
    }
}
