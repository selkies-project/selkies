/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * What a dashboard's settings controls share that is not tied to how they are
 * drawn: the menus they offer before the server's own lists arrive, the
 * debounced `settings` post, and the explicit-choice marker that tells a
 * user's pick from a value the dashboard derived or echoed.
 *
 * The cores persist every value they are told to apply, so a stored key alone
 * cannot tell a user's explicit pick from one the dashboard derived (HiDPI
 * from the resolution mode) or applied from the server (paint-over). A
 * setting that is also derived therefore carries an `_explicit_choice` marker
 * beside its value, and reads storage only through the marker: a derived
 * write never pins it, and an unmarked stored echo is dropped once the
 * conditional ladder (`conditional-settings.js`) moves on.
 * @module
 */

/** The transports a session streams over, in the order a menu lists them. */
export const STREAMING_MODES = ['websockets', 'webrtc'];
/** The transport a page starts on when neither storage nor the server names one. */
export const DEFAULT_STREAM_MODE = 'websockets';

/** Encoders offered until the server's own `encoder` list arrives. */
export const ENCODER_OPTIONS = ['h264enc', 'h265enc', 'vp8enc', 'vp9enc', 'av1enc', 'h264enc-striped', 'jpeg'];
/** The WebRTC subset of `ENCODER_OPTIONS`: the striped modes are WebSocket-only. */
export const ENCODER_OPTIONS_RTC = ['h264enc', 'h265enc', 'vp8enc', 'vp9enc', 'av1enc'];
/** `webcam_encoder` values; labels come from `displayLabel`. */
export const WEBCAM_ENCODER_OPTIONS = ['auto', 'h264', 'h265', 'vp8', 'vp9', 'av1', 'mjpeg'];
/** Encoders that take both CBR and CRF rate control. */
export const RATE_CONTROLLED_ENCODERS = ['h264enc', 'h265enc', 'vp8enc', 'vp9enc', 'av1enc', 'h264enc-striped'];
/** Rate control modes, until the server's `rate_control_mode` list arrives. */
export const RATE_CONTROL_OPTIONS = ['cbr', 'crf'];
/**
 * Mirrors the server's `audio_bitrate` enum (settings.py) so the slider never
 * offers a value the server rejects; 510000 is libopus's maximum.
 */
export const AUDIO_BITRATE_OPTIONS = [32000, 48000, 64000, 96000, 128000, 192000, 256000, 320000, 384000, 510000];
/** Default `audio_bitrate` in bits per second. */
export const DEFAULT_AUDIO_BITRATE = 128000;
/** Default `video_bitrate` in kbps, the unit the slider and the wire share. */
export const DEFAULT_VIDEO_BITRATE = 8000;
/** Resolutions the preset menu offers, as `WxH`. */
export const RESOLUTION_PRESETS = [
  '1920x1080', '1280x720', '1366x768', '1920x1200', '2560x1440', '3840x2160',
  '1024x768', '800x600', '640x480', '320x240',
];

/**
 * The transport a page starts on: its stored pick, else the mode the server
 * reported before the core loaded (`window.__SELKIES_STREAMING_MODE__`), else
 * the default.
 * @param {(key: string) => (string|null)} readStored Reads a stored setting by name.
 * @returns {string}
 */
export function initialStreamMode(readStored) {
  const saved = readStored('stream_mode');
  if (saved && STREAMING_MODES.includes(saved)) return saved;
  const runtime = typeof window !== 'undefined' ? window.__SELKIES_STREAMING_MODE__ : undefined;
  if (runtime && STREAMING_MODES.includes(runtime)) return runtime;
  return DEFAULT_STREAM_MODE;
}

/**
 * A trailing debounce of `settings` posts: a burst coalesces into one post
 * carrying every setting changed in it, each at its last value, so a derived
 * change never drops the one that caused it.
 * @param {number} delay Milliseconds of quiet before the post.
 * @returns {(setting: Object<string, *>) => void}
 */
export function settingsPoster(delay) {
  let pending = {};
  let timeoutId;
  return (setting) => {
    Object.assign(pending, setting);
    clearTimeout(timeoutId);
    timeoutId = setTimeout(() => {
      const settings = pending;
      pending = {};
      window.postMessage({ type: 'settings', settings }, window.location.origin);
    }, delay);
  };
}

/** Suffix of the marker an explicit choice writes beside its value. */
export const EXPLICIT_CHOICE_SUFFIX = '_explicit_choice';

/**
 * The marker key, suffixed onto the already-prefixed value key so it inherits
 * the per-display suffix and a secondary display keeps its own choice.
 * @param {(key: string) => string} keyFor A setting's storage key.
 * @param {{storageKey: string}} spec The setting's spec.
 * @returns {string}
 */
export function explicitChoiceKey(keyFor, spec) {
  return `${keyFor(spec.storageKey)}${EXPLICIT_CHOICE_SUFFIX}`;
}

/**
 * Whether the user chose the setting's stored value.
 * @param {(key: string) => string} keyFor A setting's storage key.
 * @param {{storageKey: string}} spec The setting's spec.
 * @returns {boolean}
 */
export function isExplicitChoice(keyFor, spec) {
  try {
    return window.localStorage.getItem(explicitChoiceKey(keyFor, spec)) === 'true';
  } catch (e) {
    return false;
  }
}

/**
 * A storage reader that answers only while the setting's marker stands, for
 * the settings that are also derived.
 * @param {(key: string) => string} keyFor A setting's storage key.
 * @param {{storageKey: string}} spec The setting's spec.
 * @returns {(key: string) => (string|null)}
 */
export function explicitStoredReader(keyFor, spec) {
  return (key) => (isExplicitChoice(keyFor, spec) ? window.localStorage.getItem(keyFor(key)) : null);
}

/**
 * The nearest even number at or below `value`, which every encoder's
 * subsampling needs of a dimension; 0 for anything that is not a number.
 * @param {number|string} value
 * @returns {number}
 */
export function roundDownToEven(value) {
  const n = parseInt(String(value), 10);
  if (isNaN(n)) return 0;
  return Math.floor(n / 2) * 2;
}

/**
 * The manual resolution in force, as the cores resolve it: a deployment that
 * forces one and names its size wins, since the core takes the server's over
 * the client's there; otherwise the stored pick, and zeroes for a session
 * sized to its window.
 * @param {object|null} serverSettings The `serverSettings` payload.
 * @param {(key: string) => (string|null)} readStored Reads a stored setting by name.
 * @returns {{w: number, h: number}}
 */
export function manualResolution(serverSettings, readStored) {
  const forced = !!(serverSettings && serverSettings.manual_resolution && serverSettings.manual_resolution.value === true);
  const server = {
    w: parseInt(serverSettings && serverSettings.manual_width && serverSettings.manual_width.value, 10) || 0,
    h: parseInt(serverSettings && serverSettings.manual_height && serverSettings.manual_height.value, 10) || 0,
  };
  if (forced && server.w > 0 && server.h > 0) return server;
  return {
    w: parseInt(readStored('manual_width') || '', 10) || 0,
    h: parseInt(readStored('manual_height') || '', 10) || 0,
  };
}
