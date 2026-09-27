/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * What both dashboards offer touch clients beside their soft modifier keys:
 * the trackpad speeds, as factors on the input handler's accelerated trackpad
 * travel (`Input.setTrackpadSpeed`).
 * @module
 */

/** The trackpad speeds offered, 1 being the handler's own curve. */
export const TRACKPAD_SPEEDS = [0.5, 0.75, 1, 1.5, 2, 3];

/** The storage key, under the dashboards' prefix, of the trackpad speed (the cores keep it). */
export const TRACKPAD_SPEED_KEY = 'trackpad_speed';
