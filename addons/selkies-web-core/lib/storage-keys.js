/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The localStorage key scheme the cores and the dashboard share.
 *
 * Every key is prefixed with the storage namespace derived from the page URL
 * (`getStorageAppName`), and a setting a display keeps for itself gains a
 * `_display2` suffix on the secondary display, so two displays of one session
 * stream with their own picks. One list serves both cores and the dashboard:
 * a key the running core never reads is inert under either name, while a key
 * missing from one side's list would have the secondary display write the
 * primary's key, a control that appears to work and changes the wrong display.
 * The WebSockets core alone reads the two JPEG keys, which ride its framing.
 * @module
 */

/** The settings a secondary display keeps apart from the primary's. */
export const PER_DISPLAY_SETTINGS = [
  'framerate', 'video_crf', 'video_fullcolor', 'video_10bit',
  'video_streaming_mode', 'jpeg_quality', 'paint_over_jpeg_quality', 'use_cpu',
  'video_paintover_crf', 'video_paintover_burst_frames', 'use_paint_over_quality',
  'manual_resolution', 'manual_width', 'manual_height',
  'encoder', 'scaleLocallyManual', 'use_browser_cursors', 'rate_control_mode',
  'video_bitrate', 'force_aligned_resolution', 'scaling_dpi',
];

/**
 * The localStorage key a setting lives under on a display.
 * @param {string} appName The storage namespace (`getStorageAppName`).
 * @param {string} displayId `primary` or `display2`.
 * @param {string} key The setting's name.
 * @returns {string}
 */
export function settingStorageKey(appName, displayId, key) {
  const prefixed = `${appName}_${key}`;
  return (displayId === 'display2' && PER_DISPLAY_SETTINGS.includes(key)) ? `${prefixed}_display2` : prefixed;
}
