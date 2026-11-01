/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The truthful outcome of a Wayland capture start, on either transport.
//!
//! A Wayland start only enqueues a compositor command, so its result is not known when the call
//! returns. The realized-geometry read is answered only once the queued start ran, which makes
//! it the barrier behind which `is_capturing` and `capture_state` are authoritative: a start
//! that left no live pipeline (a missing display id, a dead host compositor) is a failure the
//! caller has to surface to the client, while a live capture that came up degraded (a hardware
//! encoder that fell back to the CPU, a host connect that failed into local compositing) carries
//! a caveat that is logged and still streams.

use std::sync::Arc;

use log::warn;

use crate::display::wayland_output_id;
use crate::ws::host::CaptureHandle;

/// Read `(is_live, last_error)` for `display_id`'s capture past the barrier: whether a live
/// capture exists, and the reason a start failed or the caveat a live one came up with. A barrier
/// that fails or times out does not fail the verdict by itself; liveness is still read.
pub async fn start_verdict(
    capture: &Arc<dyn CaptureHandle>,
    display_id: &str,
) -> (bool, Option<String>) {
    let output = wayland_output_id(Some(display_id));
    let probe = capture.clone();
    let read = tokio::task::spawn_blocking(move || {
        let barrier = probe.realized_geometry(output);
        let (_, last_error) = probe.capture_state(output);
        (barrier, last_error)
    })
    .await;
    let last_error = match read {
        Ok((barrier, last_error)) => {
            if let Err(e) = barrier {
                warn!(target: "display", "Wayland start barrier failed for '{display_id}': {e}");
            }
            last_error
        }
        Err(_) => None,
    };
    (capture.is_capturing(), last_error)
}

/// The reason a Wayland capture failed, or a caveat a live one came up with, read straight from
/// `capture_state`; the caller owns any ordering barrier. `None` without a capture.
pub fn last_error(capture: Option<&Arc<dyn CaptureHandle>>, display_id: &str) -> Option<String> {
    capture?
        .capture_state(wayland_output_id(Some(display_id)))
        .1
}
