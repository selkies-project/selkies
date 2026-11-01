/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The PulseAudio/PipeWire control plane over libpulse (the null sink the server captures,
//! the virtual microphone, the defaults, strayed streams) and the pcmflux capture and playback
//! wiring both transports share (`audio_control.py`, `audio_config.py`).
//!
//! Being ported; the Python module named is the specification.
