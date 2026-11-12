/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Pelorus: a graphical session described as text, for a model that cannot see it.
//!
//! Two sources are joined. The window manager says where every window is, exactly, because it
//! put it there ([`Placement`], filled by the server from its window manager's view). The
//! applications say what is inside each window over the accessibility bus ([`atspi`]): the
//! controls, their names, their state, and where each lies in coordinates of the toolkit's own
//! choosing. [`element`] turns the second into screen pixels by the first, one rule per toolkit
//! and window protocol, each found by clicking a control at the computed point and reading
//! back that it took the click; [`outline`] writes the result as the indented text a model
//! reads, every line a control and the point to click it at.
//!
//! Nothing here is a rendering of the accessibility tree for its own sake: the tree is the raw
//! material the applications happen to publish, and what is written is whatever lets a model
//! act, so containers that say nothing are folded away and a control another window covers is
//! marked as one a click would not reach.

pub mod atspi;
pub mod element;
pub mod outline;
pub mod summary;
pub mod tools;

pub use element::{Element, Placement, Protocol, Rect, States, Toolkit};
