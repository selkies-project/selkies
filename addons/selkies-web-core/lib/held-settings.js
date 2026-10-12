/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Settings a tab holds in place of its user's picks: what a fallback chose (the
 * WebSockets core's encoder fallbacks and crash ladder), and what a display the
 * page shares with another one streams with (`display_settings`). A held value
 * is stored where the pick is, so the settings payloads and both dashboards
 * read it, and the pick it replaces is kept under `<key>_pick` (empty for none);
 * the tab is marked in its own sessionStorage, so its reloads keep the hold,
 * while any other tab or later visit puts the picks back. Both cores name the
 * same keys, so a tab that switches transports keeps its holds.
 * @module
 */

/** What a display streams with, as `display_settings` carries it. */
export const DISPLAY_SETTINGS = ['encoder', 'framerate', 'video_crf', 'video_fullcolor', 'video_10bit',
  'video_streaming_mode', 'jpeg_quality', 'paint_over_jpeg_quality', 'use_paint_over_quality', 'lossless_static_refinement',
  'video_paintover_crf', 'video_paintover_burst_frames', 'video_bitrate', 'rate_control_mode', 'use_cpu',
  'audio_bitrate'];
/**
 * The settings the WebSockets core's crash ladder resets, with their safe
 * values (null clears the setting). A setting the user picks again in the
 * crashing tab is theirs, which ends its hold (`releaseHeldSettings`).
 */
export const CRASH_SAFE_SETTINGS = {
  video_fullcolor: 'false', video_10bit: 'false', framerate: '60', video_crf: '25',
  manual_resolution: 'false', manual_width: null, manual_height: null,
};
/** Every setting but the encoder a tab can hold. */
export const HELD_SETTINGS = [...new Set([...Object.keys(CRASH_SAFE_SETTINGS),
  ...DISPLAY_SETTINGS.filter((name) => name !== 'encoder')])];

/**
 * The storage keys a page's holds live under.
 * @param {function(string): string} keyFor A setting's storage key.
 * @param {string} appName The storage namespace.
 * @returns {{encoderPick: string, fallbackTab: string, displayTab: string}}
 */
export const holdKeys = (keyFor, appName) => ({
  encoderPick: `${keyFor('encoder')}_pick`,
  fallbackTab: `${keyFor('encoder')}_pick_tab`,
  displayTab: `${appName}_display_settings_tab`,
});

/** The key a setting's pick is kept under while a hold stands in for it. */
const pickKeyFor = (keyFor, name) => `${keyFor(name)}_pick`;

/**
 * Puts the picks back in a tab that holds nothing: a new tab, or a later visit.
 * @param {function(string): string} keyFor
 * @param {string} appName
 */
export function restoreHeldPicks(keyFor, appName) {
  const keys = holdKeys(keyFor, appName);
  try {
    if (window.sessionStorage.getItem(keys.fallbackTab) !== null
        || window.sessionStorage.getItem(keys.displayTab) !== null) return;
    for (const name of ['encoder', ...HELD_SETTINGS]) {
      const key = keyFor(name);
      const pick = window.localStorage.getItem(pickKeyFor(keyFor, name));
      if (pick === null) continue;
      if (pick === '') window.localStorage.removeItem(key);
      else window.localStorage.setItem(key, pick);
      window.localStorage.removeItem(pickKeyFor(keyFor, name));
    }
  } catch (e) { /* storage unavailable */ }
}

/**
 * Holds what this page's display streams with for its tab, keeping the picks it replaces.
 * @param {function(string): string} keyFor
 * @param {string} appName
 * @param {Object<string, *>} values A `display_settings` payload.
 * @param {string} stamp The tab's mark: when and why.
 * @returns {Object<string, *>} The values that differ from what the tab held.
 */
export function holdDisplaySettings(keyFor, appName, values, stamp) {
  const changed = {};
  try {
    for (const name of DISPLAY_SETTINGS) {
      if (values[name] === undefined || values[name] === null) continue;
      const key = keyFor(name);
      const value = String(values[name]);
      if (window.localStorage.getItem(key) === value) continue;
      const pickKey = pickKeyFor(keyFor, name);
      if (window.localStorage.getItem(pickKey) === null) {
        window.localStorage.setItem(pickKey, window.localStorage.getItem(key) ?? '');
      }
      window.localStorage.setItem(key, value);
      changed[name] = values[name];
    }
    if (Object.keys(changed).length > 0) {
      window.sessionStorage.setItem(holdKeys(keyFor, appName).displayTab, stamp);
    }
  } catch (e) { /* storage unavailable */ }
  return changed;
}

/**
 * Ends the hold of each setting in `names`: the user picked it.
 * @param {function(string): string} keyFor
 * @param {string[]} names
 */
export function releaseHeldSettings(keyFor, names) {
  try {
    for (const name of names) {
      if (name === 'encoder' || HELD_SETTINGS.includes(name)) window.localStorage.removeItem(pickKeyFor(keyFor, name));
    }
  } catch (e) { /* storage unavailable */ }
}
