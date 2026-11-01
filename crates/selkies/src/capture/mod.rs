/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The pixelflux and pcmflux glue: one capture per display delivering frames through a
//! `FrameSink` into the transports, the audio capture and playback, and the live settings
//! each capture takes (`display_utils.py`'s CaptureSettings population, `websockets_mode.py`'s
//! capture lifecycle, `webrtc_media_pipeline.py`).
//!
//! Being ported; the Python module named is the specification.
