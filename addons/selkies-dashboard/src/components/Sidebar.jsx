/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * The dashboard sidebar: every control the reference dashboard offers on top
 * of selkies-core, talking to the core through `window.postMessage` alone.
 *
 * Renders a draggable toggle handle, the core action buttons (video, audio,
 * microphone, webcam, gamepad), the soft modifier keys and virtual keyboard
 * button for touch clients, the collapsible video, screen, audio, stats,
 * clipboard, files, apps, sharing, gamepads and shortcuts sections, the
 * upload and clipboard notifications, the apps and files modals, and the
 * second-screen placement arrows.
 *
 * Messages it consumes from the core: `serverSettings` (the settings payload
 * that gates which controls render and seeds their values),
 * `pipelineStatusUpdate` and `sidebarButtonStatusUpdate`,
 * `effectiveCursorState`, `clientRoleUpdate`, `gamingModeUpdate`,
 * `toggleDashboard` and `toggleTouchGamepad` (the core-owned Ctrl+Shift+M and
 * Ctrl+Shift+G chords), `gamepadControl`, `clipboardContentUpdate`,
 * `audioDeviceSelected` (its own selection mirrored back, so the dropdowns
 * show what the core was told), `gamepadButtonUpdate` and
 * `gamepadAxisUpdate`, `fileUpload` (upload progress and every notification
 * the core raises), and `trackpadModeUpdate`.
 *
 * Messages it posts: `settings` (debounced), `pipelineControl`,
 * `gamepadControl`, `setManualResolution`, `resetResolutionToWindow`,
 * `setScaleLocally`, `setAntiAliasing`, `audioDeviceSelected`,
 * `clipboardUpdateFromUI`, `clipboardImageUpdate`, `requestFullscreen`,
 * `requestGamingMode`, `mode`, `setSynth`, `sidebarVisibilityChanged`, `TOUCH_GAMEPAD_SETUP`,
 * `TOUCH_GAMEPAD_VISIBILITY`, `touchinput:trackpad` and `touchinput:touch`,
 * plus whatever channel a conditional-settings spec propagates through. The
 * soft keys dispatch synthetic `KeyboardEvent`s on `window`, and the files
 * section dispatches the `requestFileUpload` DOM event.
 *
 * `window` state it reads: `system_stats`, `gpu_stats`, `fps`,
 * `currentAudioLevel`, `network_stats`, `webrtcInput.gamingMode`,
 * `__SELKIES_STREAMING_MODE__` and `__SELKIES_DUAL_MODE__`; it sets
 * `__selkiesModeSwitching` around a transport switch.
 *
 * Persistence: every setting lives in `localStorage` under
 * `<storageAppName>_<key>`, the keys in `PER_DISPLAY_SETTINGS` gaining a
 * `_display2` suffix on the `#display2` hash so a secondary display keeps its
 * own values; an explicit user choice of a derived setting writes a
 * `_explicit_choice` marker beside its value. The theme is stored unprefixed
 * because it is a per-browser preference.
 * @module
 */
import { useState, useEffect, useCallback, useId, useMemo, useRef } from "react";
import { displayLabel, decodableEncoders, getRoutePrefix, getStorageAppName } from "../../../selkies-web-core/lib/util.js";
import { withSessionToken } from "../../../selkies-web-core/lib/session-token.js";
import { resolveSpec, isSettingPinned, HIDPI_SPEC, RATE_CONTROL_SPEC,
  USE_BROWSER_CURSORS_SPEC, VIDEO_FULLCOLOR_SPEC, VIDEO_STREAMING_MODE_SPEC,
  USE_PAINT_OVER_QUALITY_SPEC, USE_CPU_SPEC, FORCE_ALIGNED_RESOLUTION_SPEC } from "../../../selkies-web-core/lib/conditional-settings.js";
import GamepadVisualizer from "./GamepadVisualizer";
import { getTranslator } from "../translations";
import {
  APP_COMMAND_STATE_EVENT,
  INSTALLED_APPS_ROLLBACK_EVENT,
  pendingAppAction,
  postAppCommand,
  readInstalledApps,
  resolveFailedAppCommand,
  writeInstalledApps,
} from "../../../selkies-web-core/lib/app-commands.js";
import * as yaml from "js-yaml";

const urlHash = window.location.hash;
const displayId = urlHash.startsWith('#display2') ? 'display2' : 'primary';

/**
 * Union of both streaming cores' `PER_DISPLAY_SETTINGS` lists, so the
 * dashboard and whichever core is running agree on which keys get the
 * `_display2` suffix. The websockets core alone owns `jpeg_quality` and
 * `paint_over_jpeg_quality` (JPEG is websockets framing); a key the running
 * core ignores is inert, while a missing one would make the secondary display
 * write the primary's key.
 */
const PER_DISPLAY_SETTINGS = [
    'framerate', 'video_crf', 'video_fullcolor',
    'video_streaming_mode', 'jpeg_quality', 'paint_over_jpeg_quality', 'use_cpu',
    'video_paintover_crf', 'video_paintover_burst_frames', 'use_paint_over_quality',
    'manual_resolution', 'manual_width', 'manual_height', 'encoder',
    'scaleLocallyManual', 'use_browser_cursors', 'rate_control_mode',
    'video_bitrate', 'force_aligned_resolution'
];

const encoderOptions = [
  "h264enc",
  "h264enc-striped",
  "jpeg",
];

/**
 * WebRTC encoders offered before the server payload arrives; its `encoder`
 * allowed list is already filtered to what the WebRTC pipeline produces
 * (pixelflux emits H.264 only).
 */
const encoderOptionsWR = [
  "h264enc",
]

/** `webcam_encoder` values; labels come from `displayLabel`. */
const webcamEncoderOptions = ["auto", "h264", "vp8", "mjpeg"];

const rateControlOptions = ["cbr", "crf"];

const commonResolutionValues = [
  "",
  "1920x1080",
  "1280x720",
  "1366x768",
  "1920x1200",
  "2560x1440",
  "3840x2160",
  "1024x768",
  "800x600",
  "640x480",
  "320x240",
];

const dpiScalingOptions = [
  { label: "100%", value: 96 },
  { label: "125%", value: 120 },
  { label: "150%", value: 144 },
  { label: "175%", value: 168 },
  { label: "200%", value: 192 },
  { label: "225%", value: 216 },
  { label: "250%", value: 240 },
  { label: "275%", value: 264 },
  { label: "300%", value: 288 },
];
/**
 * Browser language, resolved once: language, form factor and display density
 * are fixed for the life of the document, so resolving them at module scope
 * makes them available to the first render and a phone gets the mobile layout
 * without a repaint.
 */
const BROWSER_LANG_TAG =
  (typeof navigator !== "undefined" &&
    (navigator.language || navigator.userLanguage)) || "en";
const BROWSER_PRIMARY_LANG = BROWSER_LANG_TAG.split("-")[0].toLowerCase();

const IS_MOBILE_CLIENT =
  typeof window !== "undefined" &&
  !!((navigator.userAgentData && navigator.userAgentData.mobile) ||
    /Mobi|Android|iPhone|iPad|iPod|BlackBerry|IEMobile|Opera Mini/i.test(
      navigator.userAgent
    ));

/**
 * The `scaling_dpi` default derived from the local display scaling
 * (`devicePixelRatio`), so the remote desktop's fonts and UI match the local
 * environment; an explicit picker value wins over it. Same formula as the
 * core's `autoDeriveDpi` and independent of the resolution. Snapping to the
 * nearest option puts a density the options do not name on the closest one
 * and clamps at both ends.
 * @returns {number} One of the `dpiScalingOptions` values.
 */
const deriveDpiFromDpr = () => {
  const dpr = (typeof window !== "undefined" ? window.devicePixelRatio || 1 : 1);
  const target = Math.round(dpr * 4) * 24;
  return dpiScalingOptions.reduce((prev, curr) =>
    Math.abs(curr.value - target) < Math.abs(prev.value - target) ? curr : prev
  ).value;
};

/** Seeds the picker with the value the server is told, so the two never disagree. */
const DEVICE_DPI = deriveDpiFromDpr();

const STATS_READ_INTERVAL_MS = 500;
const DEFAULT_FRAMERATE = 60;
const DEFAULT_JPEG_QUALITY = 40;
const DEFAULT_PAINT_OVER_JPEG_QUALITY = 90;
const DEFAULT_USE_CPU = false;
const DEFAULT_H264_PAINTOVER_CRF = 18;
const DEFAULT_USE_PAINT_OVER_QUALITY = true;
const DEFAULT_ENCODER = encoderOptions[0];
const DEFAULT_VIDEO_CRF = 25;
const DEFAULT_SCALE_LOCALLY = true;
const DEFAULT_ENABLE_BINARY_CLIPBOARD = true;
const REPO_BASE_URL =
  "https://raw.githubusercontent.com/linuxserver/proot-apps/master/metadata/";
const METADATA_URL = `${REPO_BASE_URL}metadata.yml`;
const IMAGE_BASE_URL = `${REPO_BASE_URL}img/`;
const METADATA_FETCH_TIMEOUT_MS = 10000;

const MAX_NOTIFICATIONS = 3;
const NOTIFICATION_TIMEOUT_SUCCESS = 5000;
const NOTIFICATION_TIMEOUT_ERROR = 8000;
const NOTIFICATION_FADE_DURATION = 500;

const TOUCH_GAMEPAD_HOST_DIV_ID = "touch-gamepad-host";

const STREAM_MODE_WEBRTC = "webrtc";
const STREAM_MODE_WEBSOCKETS = "websockets";
const STREAMING_MODES= [STREAM_MODE_WEBSOCKETS, STREAM_MODE_WEBRTC]
const DEFAULT_STREAM_MODE = STREAM_MODE_WEBSOCKETS;
/** Audio bitrate default in bps, matching the server and the wish dashboard. */
const DEFAULT_AUDIO_BITRATE = 128000;
/**
 * Opus target bitrate stops mirroring the server's `audio_bitrate` allowed
 * enum (settings.py), used until `serverSettings` arrive; 510k is libopus's
 * hard maximum.
 */
const audioBitrateOptions = [32000, 48000, 64000, 96000, 128000, 192000, 256000, 320000, 384000, 510000];
/** Video bitrate default in kbps. */
const DEFAULT_VIDEO_BITRATE = 8000;
const RATE_CONTROL_CBR = "cbr";
const RATE_CONTROL_CRF = "crf";

/** Sub-Mbps CBR stops in kbps for constrained links, ahead of the 1000-kbps steps. */
const SUB_MBPS_BITRATE_STEPS = [100, 250, 500, 750];
/**
 * CBR stops above 100000 kbps: per-1000 granularity stops mattering there and
 * a 1000-position slider would be unusable.
 */
const COARSE_MBPS_BITRATE_STEPS = [150000, 200000, 300000, 400000, 500000, 750000, 1000000];


/**
 * Formats a byte count with a binary unit.
 * @param {number|null|undefined} bytes Byte count; empty or zero yields the zero label.
 * @param {number} [decimals=2] Fraction digits, clamped at zero.
 * @param {object} [rawDict] Translation table supplying `zeroBytes` and `byteUnits`.
 * @returns {string}
 */
function formatBytes(bytes, decimals = 2, rawDict) {
  const zeroBytesText = rawDict?.zeroBytes || "0 Bytes";
  if (bytes === null || bytes === undefined || bytes === 0)
    return zeroBytesText;
  const k = 1024;
  const dm = decimals < 0 ? 0 : decimals;
  const sizes = rawDict?.byteUnits || [
    "Bytes",
    "KB",
    "MB",
    "GB",
    "TB",
    "PB",
    "EB",
    "ZB",
    "YB",
  ];
  const i = Math.floor(Math.log(bytes) / Math.log(k));
  const unitIndex = Math.min(i, sizes.length - 1);
  return (
    parseFloat((bytes / Math.pow(k, i)).toFixed(dm)) + " " + sizes[unitIndex]
  );
}

/** Stroke dash offset that fills a gauge ring to `percentage` (clamped to 0 to 100). */
const calculateGaugeOffset = (percentage, radius, circumference) => {
  const clampedPercentage = Math.max(0, Math.min(100, percentage || 0));
  return circumference * (1 - clampedPercentage / 100);
};

/** Parses a dimension and rounds it down to an even number; non-numbers become 0. */
const roundDownToEven = (num) => {
  const n = parseInt(num, 10);
  if (isNaN(n)) return 0;
  return Math.floor(n / 2) * 2;
};

/**
 * Trailing debounce: only the last call within `delay` milliseconds runs.
 * @param {Function} func
 * @param {number} delay
 * @returns {Function}
 */
function debounce(func, delay) {
  let timeoutId;
  return function (...args) {
    const context = this;
    clearTimeout(timeoutId);
    timeoutId = setTimeout(() => {
      func.apply(context, args);
    }, delay);
  };
}

const CopyIcon = () => (
  <svg viewBox="0 0 24 24" fill="currentColor" width="16" height="16" style={{ display: 'block' }}>
    <path d="M16 1H4c-1.1 0-2 .9-2 2v14h2V3h12V1zm3 4H8c-1.1 0-2 .9-2 2v14c0 1.1.9 2 2 2h11c1.1 0 2-.9 2-2V7c0-1.1-.9-2-2-2zm0 16H8V7h11v14z"/>
  </svg>
);
/**
 * Crosshair marking gaming mode, the wish dashboard's icon for it: one glyph
 * names the control in both front ends and reads as a target, not a plus sign.
 * Drawn to the ink span of the fullscreen brackets beside it, which cover only
 * the middle of their box, so the header pair reads as one size.
 */
const GamingModeIcon = () => (
  <svg className="stroke-icon" viewBox="0 0 24 24" stroke="currentColor" strokeWidth="2" fill="none"
    width="18" height="18" strokeLinecap="round" strokeLinejoin="round">
    <circle cx="12" cy="12" r="6" />
    <path d="M18 12h-2.4M8.4 12H6M12 8.4V6M12 18v-2.4" />
  </svg>
);
const AppsIcon = () => (
  <svg viewBox="0 0 24 24" fill="currentColor" width="20" height="20">
    <path d="M4 8h4V4H4v4zm6 12h4v-4h-4v4zm-6 0h4v-4H4v4zm0-6h4v-4H4v4zm6 0h4v-4h-4v4zm6-10v4h4V4h-4zm-6 4h4V4h-4v4zm6 6h4v-4h-4v4zm0 6h4v-4h-4v4z" />
  </svg>
);
/* Padded viewBox: the glyph inks its full box where its row neighbours ink
   about four fifths, and drawn as-is it reads as the larger tile. */
const KeyboardIcon = () => (
  <svg 
    xmlns="http://www.w3.org/2000/svg" 
    viewBox="-65 -65 620 620" 
    fill="currentColor" 
    width="24" 
    height="24"
  >
    <path d="M251.2 193.5v-53.7a10.5 10.5 0 0 1 10.5-10.5h119.4c21 0 38.1-17.1 38.1-38.1s-17.1-38.1-38.1-38.1H129.5c-5.4 0-10.1 4.3-10.1 10.1s4.3 10.1 10.1 10.1h251.6c10.1 0 17.9 8.2 17.9 17.9 0 10.1-8.2 17.9-17.9 17.9H261.7c-16.7 0-30.3 13.6-30.3 30.3v53.3H0v244.2h490V193.5H251.2zm-19 28h15.6c5.4 0 10.1 4.3 10.1 10.1s-4.3 10.1-10.1 10.1h-15.6c-5.4 0-10.1-4.3-10.1-10.1s4.6-10.1 10.1-10.1zm-28.8 104.2h-15.6c-5.4 0-10.1-4.3-10.1-10.1 0-5.4 4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1 0 5.4-4.7 10.1-10.1 10.1zm10.1 27.2c0 5.4-4.3 10.1-10.1 10.1h-15.6c-5.4 0-10.1-4.3-10.1-10.1 0-5.4 4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.7 10.1 10.1zM203.4 288h-15.6c-5.4 0-10.1-4.3-10.1-10.1s4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1s-4.7 10.1-10.1 10.1zm-17.1-66.5h15.6c5.4 0 10.1 4.3 10.1 10.1s-4.3 10.1-10.1 10.1h-15.6c-5.4 0-10.1-4.3-10.1-10.1s4.6-10.1 10.1-10.1zm-45.9 0H156c5.4 0 10.1 4.3 10.1 10.1s-4.3 10.1-10.1 10.1h-15.6c-5.4 0-10.1-4.3-10.1-10.1s4.6-10.1 10.1-10.1zm-1.6 46.6h15.6c5.4 0 10.1 4.3 10.1 10.1s-4.3 10.1-10.1 10.1h-15.6c-5.4 0-10.1-4.3-10.1-10.1s4.7-10.1 10.1-10.1zm0 37.4h15.6c5.4 0 10.1 4.3 10.1 10.1 0 5.4-4.3 10.1-10.1 10.1h-15.6c-5.4 0-10.1-4.3-10.1-10.1 0-5.5 4.7-10.1 10.1-10.1zm0 37.3h15.6c5.4 0 10.1 4.3 10.1 10.1 0 5.4-4.3 10.1-10.1 10.1h-15.6c-5.4 0-10.1-4.3-10.1-10.1 0-5.4 4.7-10.1 10.1-10.1zM94.5 221.5h15.6c5.4 0 10.1 4.3 10.1 10.1s-4.3 10.1-10.1 10.1H94.5c-5.4 0-10.1-4.3-10.1-10.1s4.7-10.1 10.1-10.1zm-5.1 46.6H105c5.4 0 10.1 4.3 10.1 10.1s-4.3 10.1-10.1 10.1H89.4c-5.4 0-10.1-4.3-10.1-10.1s4.7-10.1 10.1-10.1zm0 37.4H105c5.4 0 10.1 4.3 10.1 10.1 0 5.4-4.3 10.1-10.1 10.1H89.4c-5.4 0-10.1-4.3-10.1-10.1.4-5.5 4.7-10.1 10.1-10.1zm0 37.3H105c5.4 0 10.1 4.3 10.1 10.1 0 5.4-4.3 10.1-10.1 10.1H89.4c-5.4 0-10.1-4.3-10.1-10.1.4-5.4 4.7-10.1 10.1-10.1zM56 400.4H40.4c-5.4 0-10.1-4.3-10.1-10.1 0-5.4 4.3-10.1 10.1-10.1H56c5.4 0 10.1 4.3 10.1 10.1-.4 5.4-4.7 10.1-10.1 10.1zm0-37.4H40.4c-5.4 0-10.1-4.3-10.1-10.1 0-5.4 4.3-10.1 10.1-10.1H56c5.4 0 10.1 4.3 10.1 10.1-.4 5.5-4.7 10.1-10.1 10.1zm0-37.3H40.4c-5.4 0-10.1-4.3-10.1-10.1 0-5.4 4.3-10.1 10.1-10.1H56c5.4 0 10.1 4.3 10.1 10.1-.4 5.4-4.7 10.1-10.1 10.1zm0-37.7H40.4c-5.4 0-10.1-4.3-10.1-10.1s4.3-10.1 10.1-10.1H56c5.4 0 10.1 4.3 10.1 10.1S61.4 288 56 288zm0-46.7H40.4c-5.4 0-10.1-4.3-10.1-10.1s4.3-10.1 10.1-10.1H56c5.4 0 10.1 4.3 10.1 10.1s-4.7 10.1-10.1 10.1zm196.8 159.1H89.4c-5.4 0-10.1-4.3-10.1-10.1 0-5.4 4.3-10.1 10.1-10.1h163.3c5.4 0 10.1 4.3 10.1 10.1.1 5.4-4.6 10.1-10 10.1zm0-37.4h-15.6c-5.4 0-10.1-4.3-10.1-10.1 0-5.4 4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1 0 5.5-4.7 10.1-10.1 10.1zm0-37.3h-15.6c-5.4 0-10.1-4.3-10.1-10.1 0-5.4 4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1 0 5.4-4.7 10.1-10.1 10.1zm0-37.7h-15.6c-5.4 0-10.1-4.3-10.1-10.1s4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1s-4.7 10.1-10.1 10.1zm49.4 112.4h-15.6c-5.4 0-10.1-4.3-10.1-10.1 0-5.4 4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1-.4 5.4-4.7 10.1-10.1 10.1zm0-37.4h-15.6c-5.4 0-10.1-4.3-10.1-10.1 0-5.4 4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1-.4 5.5-4.7 10.1-10.1 10.1zm0-37.3h-15.6c-5.4 0-10.1-4.3-10.1-10.1 0-5.4 4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1-.4 5.4-4.7 10.1-10.1 10.1zm0-37.7h-15.6c-5.4 0-10.1-4.3-10.1-10.1s4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1s-4.7 10.1-10.1 10.1zm10.1-46.7h-15.6c-5.4 0-10.1-4.3-10.1-10.1s4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1s-4.7 10.1-10.1 10.1zm38.9 159.1h-15.6c-5.4 0-10.1-4.3-10.1-10.1 0-5.4 4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1 0 5.4-4.7 10.1-10.1 10.1zm0-37.4h-15.6c-5.4 0-10.1-4.3-10.1-10.1 0-5.4 4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1 0 5.5-4.7 10.1-10.1 10.1zm0-37.3h-15.6c-5.4 0-10.1-4.3-10.1-10.1 0-5.4 4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1 0 5.4-4.7 10.1-10.1 10.1zm0-37.7h-15.6c-5.4 0-10.1-4.3-10.1-10.1s4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1s-4.7 10.1-10.1 10.1zm6.6-46.7h-15.6c-5.4 0-10.1-4.3-10.1-10.1s4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1s-4.3 10.1-10.1 10.1zm42.8 159.1H385c-5.4 0-10.1-4.3-10.1-10.1 0-5.4 4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1-.4 5.4-4.7 10.1-10.1 10.1zm0-37.4H385c-5.4 0-10.1-4.3-10.1-10.1 0-5.4 4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1-.4 5.5-4.7 10.1-10.1 10.1zm0-37.3H385c-5.4 0-10.1-4.3-10.1-10.1 0-5.4 4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1-.4 5.4-4.7 10.1-10.1 10.1zm0-37.7H385c-5.4 0-10.1-4.3-10.1-10.1s4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1S406 288 400.6 288zm3.1-46.7h-15.6c-5.4 0-10.1-4.3-10.1-10.1s4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1s-4.3 10.1-10.1 10.1zm45.9 159.1H434c-5.4 0-10.1-4.3-10.1-10.1 0-5.4 4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1 0 5.4-4.7 10.1-10.1 10.1zm0-37.4H434c-5.4 0-10.1-4.3-10.1-10.1 0-5.4 4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1 0 5.5-4.7 10.1-10.1 10.1zm0-37.3H434c-5.4 0-10.1-4.3-10.1-10.1 0-5.4 4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1 0 5.4-4.7 10.1-10.1 10.1zm0-37.7H434c-5.4 0-10.1-4.3-10.1-10.1s4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1S455 288 449.6 288zm0-46.7H434c-5.4 0-10.1-4.3-10.1-10.1s4.3-10.1 10.1-10.1h15.6c5.4 0 10.1 4.3 10.1 10.1s-4.7 10.1-10.1 10.1z" />
  </svg>
);
/* Padded viewBox: this glyph inks the full 24 units where its neighbours in the
   row ink about 19, and drawn at the same size it reads as the larger button. */
const ScreenIcon = () => (
  <svg viewBox="-3 -3 30 30" fill="currentColor" width="20" height="20">
    <path d="M20 18c1.1 0 1.99-.9 1.99-2L22 6c0-1.1-.9-2-2-2H4c-1.1 0-2 .9-2 2v10c0 1.1.9 2 2 2H0v2h24v-2h-4zM4 6h16v10H4V6z" />
  </svg>
);
const SpeakerIcon = () => (
  <svg viewBox="0 0 24 24" fill="currentColor" width="20" height="20">
    <path d="M3 9v6h4l5 5V4L7 9H3zm13.5 3c0-1.77-1.02-3.29-2.5-4.03v8.05c1.48-.73 2.5-2.25 2.5-4.02zM14 3.23v2.06c2.89.86 5 3.54 5 6.71s-2.11 5.85-5 6.71v2.06c4.01-.91 7-4.49 7-8.77s-2.99-7.86-7-8.77z" />
  </svg>
);
const MicrophoneIcon = () => (
  <svg viewBox="0 0 24 24" fill="currentColor" width="20" height="20">
    <path d="M12 14c1.66 0 2.99-1.34 2.99-3L15 5c0-1.66-1.34-3-3-3S9 3.34 9 5v6c0 1.66 1.34 3 3 3zm5.3-3c0 3-2.54 5.1-5.3 5.1S6.7 14 6.7 11H5c0 3.41 2.72 6.23 6 6.72V21h2v-3.28c3.28-.48 6-3.3 6-6.72h-1.7z" />
  </svg>
);
const WebcamIcon = () => (
  <svg viewBox="0 0 24 24" fill="currentColor" width="20" height="20">
    <path d="M17 10.5V7a1 1 0 0 0-1-1H4a1 1 0 0 0-1 1v10a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1v-3.5l4 4v-11l-4 4z" />
  </svg>
);
const GamepadIcon = () => (
  <svg viewBox="0 0 24 24" fill="currentColor" width="20" height="20">
    <path d="M15 7.5V2H9v5.5l3 3 3-3zM7.5 9H2v6h5.5l3-3-3-3zM9 16.5V22h6v-5.5l-3-3-3 3zM16.5 9l-3 3 3 3H22V9h-5.5z" />
  </svg>
);
const TrackpadIcon = () => (
  <svg viewBox="0 0 24 24" fill="currentColor" width="18" height="18">
    <path d="M3 5C3 3.89543 3.89543 3 5 3H19C20.1046 3 21 3.89543 21 5V15H3V5Z"/>
    <path d="M3 16H11V21H5C3.89543 21 3 20.1046 3 19V16Z"/>
    <path d="M13 16H21V19C21 20.1046 20.1046 21 19 21H13V16Z"/>
  </svg>
);
const FullscreenIcon = () => (
  <svg viewBox="0 0 24 24" fill="currentColor" width="18" height="18">
    <path d="M7 14H5v5h5v-2H7v-3zm-2-4h2V7h3V5H5v5zm12 7h-3v2h5v-5h-2v3zM14 5v2h3v3h2V5h-5z" />
  </svg>
);
const CaretDownIcon = () => (
  <svg
    viewBox="0 0 24 24"
    fill="currentColor"
    width="18"
    height="18"
    style={{ display: "block" }}
  >
    <path d="M7 10l5 5 5-5H7z" />
  </svg>
);
const CaretUpIcon = () => (
  <svg
    viewBox="0 0 24 24"
    fill="currentColor"
    width="18"
    height="18"
    style={{ display: "block" }}
  >
    <path d="M7 14l5-5 5 5H7z" />
  </svg>
);
const SpinnerIcon = () => (
  <svg
    width="18"
    height="18"
    viewBox="0 0 38 38"
    xmlns="http://www.w3.org/2000/svg"
    stroke="currentColor"
  >
    <g fill="none" fillRule="evenodd">
      <g transform="translate(1 1)" strokeWidth="3">
        <circle strokeOpacity=".3" cx="18" cy="18" r="18" />
        <path d="M36 18c0-9.94-8.06-18-18-18">
          <animateTransform
            attributeName="transform"
            type="rotate"
            from="0 18 18"
            to="360 18 18"
            dur="0.8s"
            repeatCount="indefinite"
          />
        </path>
      </g>
    </g>
  </svg>
);
/**
 * The mark from docs/assets/logo/selkies.svg. The gradient identifier is
 * per-instance: two logos sharing one identifier would leave the second
 * unpainted as soon as the instance that owns the definition unmounts.
 * @param {object} props
 * @param {number} [props.width=30]
 * @param {number} [props.height=30]
 * @param {string} [props.className]
 * @param {Function} props.t Translator, for the accessible label.
 */
const SelkiesLogo = ({ width = 30, height = 30, className, t, ...props }) => {
  const id = useId();
  return (
    <svg
      xmlns="http://www.w3.org/2000/svg"
      viewBox="0 0 460 460"
      width={width}
      height={height}
      className={className}
      role="img"
      aria-label={t("selkiesLogoAlt")}
      {...props}
    >
      <defs>
        <linearGradient id={id} x1="0" y1="1" x2="0" y2="0">
          <stop offset="0.4293" stopColor="#D5499A" />
          <stop offset="0.6263" stopColor="#C34AA2" />
          <stop offset="0.9945" stopColor="#7967C5" />
        </linearGradient>
      </defs>
      <g transform="translate(0,460) scale(0.1,-0.1)">
        <path
          fill={`url(#${id})`}
          fillRule="evenodd"
          d="M3570 4078 l-65 -19 -125 -36 -125 -35 -95 -28 -95 -28 -95 -26 -95 -27 -45 -12 -45 -12 -355 -707 -354 -708 -79 0 -79 0 5 13 6 12 34 80 35 80 46 105 46 105 0 2 0 3 -13 0 -12 0 -285 -141 -285 -142 -350 -172 -350 -173 -355 -176 -354 -176 -10 -10 -10 -10 1834 0 1834 0 179 178 179 178 -16 90 -16 89 -46 280 -45 279 -5 7 -4 7 -93 53 -92 52 -40 22 -41 22 -22 14 -22 14 0 7 0 8 340 0 340 0 19 38 19 37 96 160 96 159 0 29 0 29 -29 29 -29 29 -234 1 -233 0 -82 102 -83 102 -22 29 -23 28 -23 27 -22 26 -53 67 -52 67 -20 0 -20 -1 -65 -20z m59 -387 l83 -66 15 -11 14 -12 -58 -54 -58 -55 -42 -37 -42 -38 -128 58 -128 57 -8 7 -8 7 73 63 73 64 10 10 10 11 43 38 43 38 12 -7 12 -7 84 -66z M54 1558 l6 -33 14 -105 15 -105 16 -120 17 -120 1 -1 2 -1 135 -93 135 -94 65 -46 65 -47 83 -57 83 -58 4 4 4 4 -14 59 -14 60 -27 120 -26 120 -19 85 -19 85 -5 23 -5 22 1075 0 1075 0 0 13 -1 12 -77 150 -77 150 -1258 3 -1258 2 5 -32z M2940 1584 l0 -6 122 -243 122 -243 -59 -68 -60 -68 -25 -26 -25 -27 -35 -39 -35 -39 -62 -71 -63 -72 0 -6 0 -6 199 0 199 0 183 136 184 137 124 91 123 91 30 25 30 26 -68 92 -69 92 -85 115 -85 115 -322 0 -323 0 0 -6z"
        />
      </g>
    </svg>
  );
};

/**
 * Session cache of the fetched proot-apps catalog: AppsModal is conditionally
 * mounted, so each open is a fresh mount, and a hit here skips the network.
 */
let cachedAppData = null;

/**
 * Audio level of the WebRTC stream's audio track through a dashboard-owned
 * AnalyserNode, never routed to a destination so playback is unaffected. The
 * websockets worklet path exposes `window.currentAudioLevel` instead.
 * @param {{current: object|null}} meterRef Ref holding the analyser, rebuilt when the stream changes.
 * @returns {number|null} RMS in 0 to 1, or `null` without an audio track.
 */
function readStreamAudioLevel(meterRef) {
  const el = document.getElementById("stream");
  const ms = el && el.srcObject;
  if (!ms || typeof ms.getAudioTracks !== "function" || ms.getAudioTracks().length === 0) {
    return null;
  }
  let m = meterRef.current;
  if (!m || m.stream !== ms) {
    try {
      if (m && m.ctx) m.ctx.close();
      const Ctx = window.AudioContext || window.webkitAudioContext;
      const ctx = new Ctx();
      const analyser = ctx.createAnalyser();
      analyser.fftSize = 512;
      ctx.createMediaStreamSource(ms).connect(analyser);
      m = { ctx, analyser, data: new Uint8Array(analyser.fftSize), stream: ms };
      meterRef.current = m;
    } catch {
      return null;
    }
  }
  m.analyser.getByteTimeDomainData(m.data);
  let sum = 0;
  for (let i = 0; i < m.data.length; i++) {
    const v = (m.data[i] - 128) / 128;
    sum += v * v;
  }
  return Math.sqrt(sum / m.data.length);
}

/**
 * Catalog of proot-apps with install, remove, update and launch actions,
 * posted as app commands through app-commands.js.
 * @param {object} props
 * @param {boolean} props.isOpen Renders nothing while false.
 * @param {() => void} props.onClose
 * @param {Function} props.t Translator.
 * @param {boolean} props.commandsAvailable Whether the server accepts remote commands; actions are disabled otherwise.
 * @param {boolean} props.commandsKnown Whether `serverSettings` have arrived, so the disabled notice is only shown once known.
 */
function AppsModal({ isOpen, onClose, t, commandsAvailable, commandsKnown }) {
  const [appData, setAppData] = useState(cachedAppData);
  const [isLoading, setIsLoading] = useState(false);
  const [error, setError] = useState(null);
  const [fetchAttempt, setFetchAttempt] = useState(0);
  const [searchTerm, setSearchTerm] = useState("");
  const [selectedApp, setSelectedApp] = useState(null);
  const [installedApps, setInstalledApps] = useState(readInstalledApps);
  const [commandTick, setCommandTick] = useState(0);

  useEffect(() => {
    writeInstalledApps(installedApps);
  }, [installedApps]);

  /* The running set lives in app-commands.js; this only re-reads it. */
  useEffect(() => {
    const onCommandState = () => setCommandTick((tick) => tick + 1);
    window.addEventListener(APP_COMMAND_STATE_EVENT, onCommandState);
    return () =>
      window.removeEventListener(APP_COMMAND_STATE_EVENT, onCommandState);
  }, []);

  /**
   * A failed install or remove already rolled the stored list back; mirror
   * it into this mounted list so the badge flips without a remount.
   */
  useEffect(() => {
    const onRollback = (event) => {
      const { app, action } = event.detail || {};
      if (action === "install")
        setInstalledApps((prev) => prev.filter((name) => name !== app));
      else if (action === "remove")
        setInstalledApps((prev) =>
          prev.includes(app) ? prev : [...prev, app]
        );
    };
    window.addEventListener(INSTALLED_APPS_ROLLBACK_EVENT, onRollback);
    return () =>
      window.removeEventListener(INSTALLED_APPS_ROLLBACK_EVENT, onRollback);
  }, []);

  /**
   * Catalog fetch: one attempt per modal open plus explicit Retry bumps of
   * `fetchAttempt`; a failure settles into the error view rather than
   * refetching. The fetch is aborted after a timeout and on close or unmount,
   * and the cleanup's `active` flag suppresses any late setState. The loading
   * flag is state set here rather than derived during render: it marks the
   * transition into a fetch, which a Retry has to re-enter.
   */
  useEffect(() => {
    if (!isOpen || appData) return;
    const controller = new AbortController();
    const timeoutId = window.setTimeout(
      () => controller.abort(),
      METADATA_FETCH_TIMEOUT_MS
    );
    let active = true;
    // eslint-disable-next-line react-hooks/set-state-in-effect
    setIsLoading(true);
    setError(null);
    (async () => {
      try {
        const response = await fetch(METADATA_URL, {
          signal: controller.signal,
        });
        if (!response.ok) {
          throw new Error(`HTTP error! status: ${response.status}`);
        }
        const yamlText = await response.text();
        const parsedData = yaml.load(yamlText);
        if (!active) return;
        cachedAppData = parsedData;
        setAppData(parsedData);
      } catch (e) {
        if (!active) return;
        console.error("Failed to fetch or parse app data:", e);
        setError(
          t(
            "appsModal.errorLoading",
            "Failed to load app data. Please try again."
          )
        );
      } finally {
        clearTimeout(timeoutId);
        if (active) setIsLoading(false);
      }
    })();
    return () => {
      active = false;
      clearTimeout(timeoutId);
      controller.abort();
    };
  }, [isOpen, appData, fetchAttempt, t]);

  const handleSearchChange = (event) =>
    setSearchTerm(event.target.value.toLowerCase());
  const handleAppClick = (app) => setSelectedApp(app);
  const handleBackToGrid = () => setSelectedApp(null);

  /**
   * The apps command contract both dashboards share: app-commands.js posts
   * the selkies-proot wrapper commands and tracks them for rollback, and the
   * installed list is updated optimistically.
   */
  const handleInstall = (appName) => {
    if (!commandsAvailable) return;
    postAppCommand("install", appName);
    setInstalledApps((prev) =>
      prev.includes(appName) ? prev : [...prev, appName]
    );
  };
  const handleRemove = (appName) => {
    if (!commandsAvailable) return;
    postAppCommand("remove", appName);
    setInstalledApps((prev) => prev.filter((name) => name !== appName));
  };
  const handleUpdate = (appName) => {
    if (!commandsAvailable) return;
    postAppCommand("update", appName);
  };
  const handleLaunch = (appName) => {
    if (!commandsAvailable) return;
    postAppCommand("launch", appName);
  };

  const filteredApps =
    appData?.include?.filter(
      (app) =>
        !app.disabled &&
        (app.full_name?.toLowerCase().includes(searchTerm) ||
          app.name?.toLowerCase().includes(searchTerm) ||
          app.description?.toLowerCase().includes(searchTerm))
    ) || [];
  const isAppInstalled = (appName) => installedApps.includes(appName);

  if (!isOpen) return null;

  return (
    <div className="apps-modal">
      <button
        className="apps-modal-close"
        onClick={onClose}
        aria-label={t("appsModal.closeAlt", "Close apps modal")}
      >
        &times;
      </button>
      <div className="apps-modal-content">
        {commandsKnown && !commandsAvailable && (
          <div className="apps-modal-notice">
            <p>
              {t(
                "appsModal.commandsDisabled",
                "App installation is unavailable: this session has remote commands disabled (the server's SELKIES_COMMAND_ENABLED setting)."
              )}
            </p>
          </div>
        )}
        {isLoading && (
          <div className="apps-modal-loading">
            <SpinnerIcon />
            <p>{t("appsModal.loading", "Loading apps...")}</p>
          </div>
        )}
        {error && (
          <div className="apps-modal-error">
            <p>{error}</p>
            <button
              onClick={() => setFetchAttempt((n) => n + 1)}
              className="app-action-button install"
            >
              {t("appsModal.retryButton", "Retry")}
            </button>
          </div>
        )}
        {!isLoading && !error && appData && (
          <>
            {selectedApp ? (
              <div className="app-detail-view">
                <button
                  onClick={handleBackToGrid}
                  className="app-detail-back-button"
                >
                  &larr; {t("appsModal.backButton", "Back to list")}
                </button>
                <img
                  src={`${IMAGE_BASE_URL}${selectedApp.icon}`}
                  alt={selectedApp.full_name}
                  className="app-detail-icon"
                  onError={(e) => {
                    e.target.style.display = "none";
                  }}
                />
                <h2>{selectedApp.full_name}</h2>
                <p className="app-detail-description">
                  {selectedApp.description}
                </p>
                <div className="app-action-buttons">
                  {(() => {
                    const running =
                      commandTick >= 0 && pendingAppAction(selectedApp.name);
                    const held = !commandsAvailable || !!running;
                    const mark = (action) =>
                      `app-action-button ${action}${running === action ? " running" : ""}`;
                    return isAppInstalled(selectedApp.name) ? (
                    <>
                      <button
                        onClick={() => handleLaunch(selectedApp.name)}
                        disabled={held}
                        className={mark("install")}
                      >
                        {t("appsModal.launchButton", "Launch")}{" "}
                        {selectedApp.name}
                      </button>
                      <button
                        onClick={() => handleUpdate(selectedApp.name)}
                        disabled={held}
                        className={mark("update")}
                      >
                        {t("appsModal.updateButton", "Update")}{" "}
                        {selectedApp.name}
                      </button>
                      <button
                        onClick={() => handleRemove(selectedApp.name)}
                        disabled={held}
                        className={mark("remove")}
                      >
                        {t("appsModal.removeButton", "Remove")}{" "}
                        {selectedApp.name}
                      </button>
                    </>
                    ) : (
                      <button
                        onClick={() => handleInstall(selectedApp.name)}
                        disabled={held}
                        className={mark("install")}
                      >
                        {t("appsModal.installButton", "Install")}{" "}
                        {selectedApp.name}
                      </button>
                    );
                  })()}
                </div>
              </div>
            ) : (
              <>
                <input
                  type="text"
                  className="apps-search-bar allow-native-input"
                  placeholder={t(
                    "appsModal.searchPlaceholder",
                    "Search apps..."
                  )}
                  value={searchTerm}
                  onChange={handleSearchChange}
                />
                <div className="apps-grid">
                  {filteredApps.length > 0 ? (
                    filteredApps.map((app) => (
                      <div
                        key={app.name}
                        className="app-card"
                        onClick={() => handleAppClick(app)}
                      >
                        <img
                          src={`${IMAGE_BASE_URL}${app.icon}`}
                          alt={app.full_name}
                          className="app-card-icon"
                          loading="lazy"
                          onError={(e) => {
                            e.target.style.visibility = "hidden";
                          }}
                        />
                        <p className="app-card-name">{app.full_name}</p>
                        {isAppInstalled(app.name) && (
                          <div className="app-card-installed-badge">
                            {t("appsModal.installedBadge", "Installed")}
                          </div>
                        )}
                      </div>
                    ))
                  ) : (
                    <p>
                      {t(
                        "appsModal.noAppsFound",
                        "No apps found matching your search."
                      )}
                    </p>
                  )}
                </div>
              </>
            )}
          </>
        )}
      </div>
    </div>
  );
}

const storageAppName = getStorageAppName();
/**
 * The localStorage key for a setting: the session prefix, plus the
 * `_display2` suffix for per-display settings on the secondary display.
 * @param {string} key
 * @returns {string}
 */
const getPrefixedKey = (key) => {
  const prefixedKey = `${storageAppName}_${key}`;
  if (displayId === 'display2' && PER_DISPLAY_SETTINGS.includes(key)) {
    return `${prefixedKey}_display2`;
  }
  return prefixedKey;
};

/** Reads a setting's stored value under its prefixed key. */
const readStored = (key) => localStorage.getItem(getPrefixedKey(key));

/**
 * Marker written beside a value the user chose explicitly. The cores persist
 * every value they are told to apply, so the stored key alone cannot tell a
 * user's pick from one the dashboard derived (HiDPI from the resolution mode,
 * rate control from the encoder); the settings that are also derived read
 * storage through `readExplicitStored`, so a derived write never pins them.
 */
const EXPLICIT_CHOICE_SUFFIX = "_explicit_choice";
/**
 * The marker key, suffixed onto the already-prefixed value key so it inherits
 * the per-display suffix and a secondary display keeps its own choice.
 */
const explicitChoiceKey = (spec) => `${getPrefixedKey(spec.storageKey)}${EXPLICIT_CHOICE_SUFFIX}`;
const isExplicitChoice = (spec) => localStorage.getItem(explicitChoiceKey(spec)) === "true";
/** A storage reader for a spec that returns the stored value only when it was an explicit choice. */
const readExplicitStored = (spec) => (key) => (isExplicitChoice(spec) ? readStored(key) : null);
const readHidpiStored = readExplicitStored(HIDPI_SPEC);
const readRateControlStored = readExplicitStored(RATE_CONTROL_SPEC);
const readPaintOverStored = readExplicitStored(USE_PAINT_OVER_QUALITY_SPEC);

/**
 * Drives a conditional setting: lazy init, then a re-resolve whenever the
 * server settings or any dependency in `deps` changes, which covers the
 * server sync and the encoder or manual-resolution re-derivation uniformly.
 * The resolver honors explicit choices, so a re-resolve never clobbers a
 * pinned value. A re-resolve writes state rather than deriving during render
 * because the caller edits the value afterwards; deriving would discard that.
 * @param {object} spec A spec from conditional-settings.js.
 * @param {object|null} serverSettings The last `serverSettings` payload.
 * @param {object} ctx Resolution context the spec reads.
 * @param {Array} deps Values whose change triggers a re-resolve.
 * @param {(key: string) => string|null} [read=readStored] Storage reader.
 * @returns {[any, Function]} The value and its setter, as `useState` returns them.
 */
function useConditionalSetting(spec, serverSettings, ctx, deps, read = readStored) {
  const compute = () => resolveSpec(spec, serverSettings, ctx, read);
  const [value, setValue] = useState(compute);
  // eslint-disable-next-line react-hooks/exhaustive-deps, react-hooks/set-state-in-effect
  useEffect(() => { setValue(compute()); }, deps);
  return [value, setValue];
}

/** Height of `.toggle-handle` in Overlay.css; keep the two in sync. */
const TOGGLE_HANDLE_HEIGHT_PX = 60;
/**
 * Clamps the toggle handle's vertical position, a percentage of the viewport
 * height, so the handle stays fully inside the viewport. The handle's inline
 * `top` positions its center (Overlay.css keeps the `translateY(-50%)`), so
 * the clamp is by half the handle height. Without a finite viewport height
 * (headless, pre-layout) that half would be Infinity, so the clamp is a plain
 * 0 to 100 until a real height is known; a handle at least as tall as the
 * viewport inverts the bounds, so it is centered instead of pinned to an edge.
 * @param {number} pct
 * @returns {number}
 */
const clampToggleHandleTopPct = (pct) => {
  const safePct = Number.isFinite(pct) ? pct : 50;
  const vh = window.innerHeight;
  if (!vh || !Number.isFinite(vh)) return Math.min(100, Math.max(0, safePct));
  const halfHandlePct = (TOGGLE_HANDLE_HEIGHT_PX / 2 / vh) * 100;
  if (halfHandlePct >= 50) return 50;
  return Math.min(100 - halfHandlePct, Math.max(halfHandlePct, safePct));
};

/**
 * The sidebar component; see the module docblock for the message and storage
 * contract it implements. Renders nothing when the server hides the sidebar
 * (`ui_show_sidebar`), and viewer-role clients get no toggle handle.
 */
function Sidebar() {
  const [isOpen, setIsOpen] = useState(false);
  const [isToggleVisible, setIsToggleVisible] = useState(true);
  /**
   * Gaming mode: the fullscreen that holds the pointer and keyboard, announced
   * by the core however it was entered. Plain fullscreen keeps the dashboard
   * reachable, so only this one hides the toggle.
   */
  const [isGamingMode, setIsGamingMode] = useState(false);
  /**
   * Viewer-designated clients (shared/player URL modes, or a server-assigned
   * viewer role) get no server-wide controls such as the transport switch.
   */
  const [isViewerRole, setIsViewerRole] = useState(() => {
    const h = (typeof window !== "undefined" ? window.location.hash : "").toLowerCase();
    return h.startsWith("#shared") || /^#player[234]$/.test(h);
  });
  const toggleSidebar = () => {
    setIsOpen(!isOpen);
  };
  const isSecondaryDisplay = displayId === 'display2';
  const translator = useMemo(() => getTranslator(BROWSER_PRIMARY_LANG), []);
  // system-ui picks its face from the document language, and the script-aware
  // type rules key off it. Region included: zh-TW is not zh-CN.
  useEffect(() => {
    document.documentElement.lang = BROWSER_LANG_TAG;
  }, []);
  useEffect(() => {
    window.postMessage({ type: 'sidebarVisibilityChanged', isOpen: isOpen }, window.location.origin);
  }, [isOpen]);
  /**
   * Entering fullscreen (button, Ctrl+Shift+F, or browser UI) folds the
   * dashboard so the user lands in the session; gaming mode also hides the
   * toggle so pointer lock is not fighting it.
   */
  useEffect(() => {
    const onGamingMode = (event) => {
      if (event.source !== window || event.origin !== window.location.origin) return;
      if (!event.data || event.data.type !== "gamingModeUpdate") return;
      setIsGamingMode(!!event.data.active);
    };
    window.addEventListener("message", onGamingMode);
    return () => window.removeEventListener("message", onGamingMode);
  }, []);
  useEffect(() => {
    const foldOnFullscreen = () => {
      if (document.fullscreenElement) setIsOpen(false);
    };
    document.addEventListener("fullscreenchange", foldOnFullscreen);
    return () => document.removeEventListener("fullscreenchange", foldOnFullscreen);
  }, []);
  const currentDeviceDpi = DEVICE_DPI;
  const isMobile = IS_MOBILE_CLIENT;
  const [isTrackpadModeActive, setIsTrackpadModeActive] = useState(false);
  const [hasDetectedTouch, setHasDetectedTouch] = useState(false);
  const [heldKeys, setHeldKeys] = useState({
    Control: false,
    Alt: false,
    Meta: false,
  });
  const [isKeyboardButtonVisible, setIsKeyboardButtonVisible] = useState(true);
  const [isTouchGamepadActive, setIsTouchGamepadActive] = useState(false);
  const [isTouchGamepadSetup, setIsTouchGamepadSetup] = useState(false);
  const [availablePlacements, setAvailablePlacements] = useState(null);
  const [serverSettings, setServerSettings] = useState(null);

  const [uiTitle, setUiTitle] = useState('Selkies');
  const [uiShowLogo, setUiShowLogo] = useState(true);

  useEffect(() => {
    const handleMessage = (event) => {
      if (
        event.origin === window.location.origin &&
        event.data?.type === "serverSettings"
      ) {
        console.log("Dashboard received server settings:", event.data.payload);
        setServerSettings(event.data.payload);
      }
    };
    window.addEventListener("message", handleMessage);
    return () => {
      window.removeEventListener("message", handleMessage);
    };
  }, []);

  /**
   * Which sidebar controls the server's settings allow: a pure projection of
   * `serverSettings`. A setting that is locked, has at most one allowed value,
   * or a collapsed min/max range hides its control. The clipboard section and
   * the binary-clipboard toggle are also gated on `clipboard_enabled` (wish
   * parity): with it off the core drops writes and the textarea would be dead.
   */
  const renderableSettings = useMemo(() => {
    if (!serverSettings) return {};

    const newRenderable = {};
    const s = serverSettings;

    const isRenderable = (key) => {
        const setting = s[key];
        if (!setting) return true; 
        if (setting.locked === true) return false;
        if (setting.allowed && setting.allowed.length <= 1) return false;
        if (setting.min !== undefined && setting.max !== undefined && setting.min === setting.max) return false;
        return true;
    };

    newRenderable.videoSettings = s.ui_sidebar_show_video_settings?.value ?? true;
    newRenderable.screenSettings = s.ui_sidebar_show_screen_settings?.value ?? true;
    newRenderable.audioSettings = s.ui_sidebar_show_audio_settings?.value ?? true;
    newRenderable.stats = s.ui_sidebar_show_stats?.value ?? true;
    newRenderable.clipboard = (s.ui_sidebar_show_clipboard?.value ?? true)
      && (s.clipboard_enabled?.value ?? true);
    newRenderable.files = s.ui_sidebar_show_files?.value ?? true;
    newRenderable.apps = s.ui_sidebar_show_apps?.value ?? true;
    newRenderable.sharing = s.ui_sidebar_show_sharing?.value ?? true;
    newRenderable.gamepads = s.ui_sidebar_show_gamepads?.value ?? true;
    newRenderable.shortcuts = s.ui_sidebar_show_shortcuts?.value ?? true;
    newRenderable.fullscreen = s.ui_sidebar_show_fullscreen?.value ?? true;
    newRenderable.gamingMode = s.ui_sidebar_show_gaming_mode?.value ?? true;
    newRenderable.trackpad = s.ui_sidebar_show_trackpad?.value ?? true;
    newRenderable.keyboardButton = s.ui_sidebar_show_keyboard_button?.value ?? true;
    newRenderable.softButtons = s.ui_sidebar_show_soft_buttons?.value ?? true;
    newRenderable.coreButtons = s.ui_show_core_buttons?.value ?? true;

    newRenderable.encoder = isRenderable('encoder');
    newRenderable.framerate = isRenderable('framerate');
    newRenderable.jpeg_quality = isRenderable('jpeg_quality');
    newRenderable.paint_over_jpeg_quality = isRenderable('paint_over_jpeg_quality');
    newRenderable.video_crf = isRenderable('video_crf');
    newRenderable.videoPaintoverCRF = isRenderable('video_paintover_crf');
    newRenderable.videoPaintoverBurstFrames = isRenderable('video_paintover_burst_frames');
    newRenderable.usePaintOverQuality = isRenderable('use_paint_over_quality');
    newRenderable.videoStreamingMode = isRenderable('video_streaming_mode');
    newRenderable.videoFullColor = isRenderable('video_fullcolor');
    newRenderable.use_cpu = isRenderable('use_cpu');
    newRenderable.uiScaling = isRenderable('scaling_dpi');
    newRenderable.binaryClipboard = isRenderable('enable_binary_clipboard')
      && (s.clipboard_enabled?.value ?? true);
    newRenderable.use_browser_cursors = isRenderable('use_browser_cursors');
    newRenderable.video_bitrate = isRenderable('video_bitrate');
    newRenderable.audio_bitrate = isRenderable('audio_bitrate');

    // HiDPI on is use_css_scaling off.
    newRenderable.hidpi = isRenderable('use_css_scaling');
    newRenderable.forceAlignedResolution = isRenderable('force_aligned_resolution');

    newRenderable.enableSharing = s.enable_sharing?.value ?? true;
    newRenderable.enableShared = s.enable_shared?.value ?? true;
    newRenderable.enablePlayer2 = s.enable_player2?.value ?? true;
    newRenderable.enablePlayer3 = s.enable_player3?.value ?? true;
    newRenderable.enablePlayer4 = s.enable_player4?.value ?? true;
    newRenderable.enableDualMode = s.enable_dual_mode?.value ?? false;

    // No server-side video enable setting exists.
    newRenderable.videoToggle = true;
    newRenderable.audioToggle = isRenderable('audio_enabled');
    newRenderable.microphoneToggle = isRenderable('microphone_enabled');
    newRenderable.webcamToggle = isRenderable('webcam_enabled')
      && (s.ui_sidebar_show_webcam?.value ?? true);
    newRenderable.webcamEncoder = isRenderable('webcam_encoder') && newRenderable.webcamToggle;
    newRenderable.gamepadToggle = isRenderable('gamepad_enabled');

    // Absent from the payload means the server default, which is on.
    newRenderable.enableRateControl = s.enable_rate_control?.value ?? true;
    const ftSetting = s.file_transfers;
    newRenderable.fileUpload = ftSetting ? ftSetting.value.includes('upload') : true;
    newRenderable.fileDownload = ftSetting ? ftSetting.value.includes('download') : true;

    return newRenderable;
  }, [serverSettings]);

  /**
   * With rate control disabled the server ignores `rate_control_mode` and
   * keeps the encoder on its built-in default, so the dashboard neither pushes
   * a mode nor lets its own pick decide which quality slider is shown.
   */
  const rateControlEnabled = renderableSettings.enableRateControl ?? true;

  /**
   * Opens the secondary display in a new window, sized to `screen` when the
   * Window Management API found one in that direction.
   * @param {'up'|'down'|'left'|'right'} direction
   * @param {ScreenDetailed|null} [screen]
   */
  const launchWindow = (direction, screen = null) => {
    const url = `${window.location.href.split('#')[0]}#display2-${direction}`;
    let features = 'resizable=yes,scrollbars=yes,noopener,noreferrer';
    if (screen) {
      features += `,left=${screen.availLeft},top=${screen.availTop},width=${screen.availWidth},height=${screen.availHeight}`;
    }
    window.open(url, '_blank', features);
    setAvailablePlacements(null);
  };

  /**
   * Add Screen: places the second display on an adjacent physical screen
   * when the Window Management API reports exactly one, offers the placement
   * arrows when it reports several, and otherwise opens it to the right.
   */
  const handleAddScreenClick = async () => {
    if (!('getScreenDetails' in window)) {
      console.warn("Window Management API not supported. Opening default second screen.");
      launchWindow('right');
      return;
    }

    try {
      const screenDetails = await window.getScreenDetails();
      const currentScreen = screenDetails.currentScreen;
      const otherScreens = screenDetails.screens.filter(s => s !== currentScreen);

      if (otherScreens.length === 0) {
        console.log("No other screens detected. Opening default second screen.");
        launchWindow('right');
        return;
      }

      const placements = {};
      for (const s of otherScreens) {
        if (!placements.right && s.left >= currentScreen.left + currentScreen.width) {
          placements.right = s;
        }
        if (!placements.left && s.left + s.width <= currentScreen.left) {
          placements.left = s;
        }
        if (!placements.down && s.top >= currentScreen.top + currentScreen.height) {
          placements.down = s;
        }
        if (!placements.up && s.top + s.height <= currentScreen.top) {
          placements.up = s;
        }
      }
      
      const availableDirections = Object.keys(placements);

      if (availableDirections.length === 1) {
        const direction = availableDirections[0];
        const screen = placements[direction];
        console.log(`Auto-placing single screen to the ${direction}.`);
        launchWindow(direction, screen);
      } else if (availableDirections.length > 1) {
        console.log("Multiple placement options found. Showing arrows.");
        setAvailablePlacements(placements);
      } else {
        console.log("No adjacent screens found in cardinal directions. Opening default.");
        launchWindow('right');
      }
    } catch (err) {
      console.error("Error with Window Management API or permission denied:", err);
      launchWindow('right');
    }
  };

  useEffect(() => {
    const detectTouch = () => {
      console.log("Dashboard: First touch detected. Enabling touch-specific features.");
      setHasDetectedTouch(true);
    };
    window.addEventListener('touchstart', detectTouch, { once: true, passive: true });
    return () => {
      window.removeEventListener('touchstart', detectTouch, { once: true, passive: true });
    };
  }, []);

  useEffect(() => {
    const setRealViewportHeight = () => {
      const vh = window.innerHeight * 0.01;
      document.documentElement.style.setProperty('--vh', `${vh}px`);
    };
    window.addEventListener('resize', setRealViewportHeight);
    window.addEventListener('orientationchange', setRealViewportHeight);
    setRealViewportHeight();
    return () => {
      window.removeEventListener('resize', setRealViewportHeight);
      window.removeEventListener('orientationchange', setRealViewportHeight);
    };
  }, []);

  const { t, raw } = translator;
  /** Dispatches a synthetic keyboard event on `window` for the input core to forward. */
  const sendKeyEvent = (type, key, code, modifierState) => {
    const event = new KeyboardEvent(type, {
      key: key,
      code: code,
      ctrlKey: modifierState.Control,
      altKey: modifierState.Alt,
      metaKey: modifierState.Meta,
      bubbles: true,
      cancelable: true,
    });
    window.dispatchEvent(event);
  };
  /**
   * Soft modifier key: toggles the key held, and switches the core's synth
   * mode on with the first held modifier and off with the last release.
   */
  const handleHoldKeyClick = (key, code) => {
    const isCurrentlyHeld = heldKeys[key];
    const currentHeldCount = Object.values(heldKeys).filter(Boolean).length;
    if (!isCurrentlyHeld && currentHeldCount === 0) {
      window.postMessage({ type: 'setSynth', value: true }, window.location.origin);
    } else if (isCurrentlyHeld && currentHeldCount === 1) {
      window.postMessage({ type: 'setSynth', value: false }, window.location.origin);
    }
    const nextHeldState = {
      ...heldKeys,
      [key]: !isCurrentlyHeld,
    };
    setHeldKeys(nextHeldState);
    if (isCurrentlyHeld) {
      sendKeyEvent('keyup', key, code, nextHeldState);
      console.log(`Dashboard: Dispatched keyup for ${key} with state:`, nextHeldState);
    } else {
      sendKeyEvent('keydown', key, code, nextHeldState);
      console.log(`Dashboard: Dispatched keydown for ${key} with state:`, nextHeldState);
    }
  };
  /** Soft momentary key: a press and release carrying the held modifiers. */
  const handleOnceKeyClick = (key, code) => {
    console.log(`Dashboard: Dispatching key press for ${key} with modifiers:`, heldKeys);
    sendKeyEvent('keydown', key, code, heldKeys);
    setTimeout(() => {
      sendKeyEvent('keyup', key, code, heldKeys);
    }, 50);
  };
  const toggleKeyboardButtonVisibility = () => {
    setIsKeyboardButtonVisible(prev => !prev);
  };

  const [streamMode, setStreamMode] = useState(
    localStorage.getItem(getPrefixedKey("stream_mode")) ||
      (typeof window !== "undefined" && window.__SELKIES_STREAMING_MODE__) ||
      DEFAULT_STREAM_MODE
  );
  const [audioBitrate, setAudioBitrate] = useState(
    parseInt(localStorage.getItem(getPrefixedKey("audio_bitrate")), 10) || DEFAULT_AUDIO_BITRATE
  );
  const [videoBitrate, setVideoBitrate] = useState(
    parseInt(localStorage.getItem(getPrefixedKey("video_bitrate")), 10) || DEFAULT_VIDEO_BITRATE
  );
  const [theme, setTheme] = useState(localStorage.getItem("theme") || "dark");
  const [encoder, setEncoder] = useState(
    localStorage.getItem(getPrefixedKey("encoder")) || DEFAULT_ENCODER
  );
  const [webcamEncoderChoice, setWebcamEncoderChoice] = useState(
    localStorage.getItem(getPrefixedKey("webcam_encoder"))
  );
  const [framerate, setFramerate] = useState(
    parseInt(localStorage.getItem(getPrefixedKey("framerate")), 10) ||
      DEFAULT_FRAMERATE
  );
  const [video_crf, setVideoCRF] = useState(
    parseInt(localStorage.getItem(getPrefixedKey("video_crf")), 10) ||
      DEFAULT_VIDEO_CRF
  );
  const [videoPaintoverCRF, setVideoPaintoverCRF] = useState(
    parseInt(localStorage.getItem(getPrefixedKey("video_paintover_crf")), 10) ||
      DEFAULT_H264_PAINTOVER_CRF
  );
  const [videoPaintoverBurstFrames, setVideoPaintoverBurstFrames] = useState(
    parseInt(localStorage.getItem(getPrefixedKey("video_paintover_burst_frames")), 10) || 5
  );
  const [jpeg_quality, setJpegQuality] = useState(
    parseInt(localStorage.getItem(getPrefixedKey("jpeg_quality")), 10) ||
      DEFAULT_JPEG_QUALITY
  );
  const [paint_over_jpeg_quality, setPaintOverJpegQuality] = useState(
    parseInt(localStorage.getItem(getPrefixedKey("paint_over_jpeg_quality")), 10) ||
      DEFAULT_PAINT_OVER_JPEG_QUALITY
  );
  const [selectedDpi, setSelectedDpi] = useState(
    parseInt(localStorage.getItem(getPrefixedKey("scaling_dpi")), 10) || deriveDpiFromDpr()
  );
  const [manual_width, setManualWidth] = useState(localStorage.getItem(getPrefixedKey("manual_width")) || "");
  const [manual_height, setManualHeight] = useState(localStorage.getItem(getPrefixedKey("manual_height")) || "");
  const [scaleLocally, setScaleLocally] = useState(() => {
    const saved = localStorage.getItem(getPrefixedKey("scaleLocallyManual"));
    return saved !== null ? saved === "true" : DEFAULT_SCALE_LOCALLY;
  });
  /**
   * State the conditional settings read; rebuilt each render so the hooks
   * below re-resolve against current values when their deps change.
   * `activeEncoder` is the one encoder knob for both transports, read from
   * storage first: an out-of-set stored value is ignored by the server's own
   * fallback and re-seated by the `serverSettings` sync. `softwareH264Encoder`
   * and `useCpu` (the client's choice, else the server's) feed the
   * rate-control default.
   */
  const conditionalCtx = {
    manualActive: !!readStored("manual_width") || serverSettings?.manual_resolution?.value === true,
    streamMode,
    activeEncoder: readStored("encoder") || encoder,
    softwareH264Encoder: serverSettings?.software_h264_encoder?.value,
    useCpu: readStored("use_cpu") !== null
      ? readStored("use_cpu") === "true" : !!serverSettings?.use_cpu?.value,
    allowedRateControl: serverSettings?.rate_control_mode?.allowed || rateControlOptions,
  };
  /**
   * Each conditional setting is one hook call over a shared spec. The hook
   * owns init and server sync; client-driven changes (an explicit toggle, or
   * a dependency such as the encoder or resolution) flow through
   * `writeConditional`, which sets state, persists, and propagates uniformly.
   */
  const [hidpiEnabled, setHidpiEnabled] = useConditionalSetting(
    HIDPI_SPEC, serverSettings, conditionalCtx, [serverSettings], readHidpiStored);
  const [rateControlMode, setRateControlMode] = useConditionalSetting(
    RATE_CONTROL_SPEC, serverSettings, conditionalCtx, [serverSettings, streamMode], readRateControlStored);
  /** Paint-over's default tracks rate control, so its context carries the mode just settled on. */
  const paintOverCtx = { ...conditionalCtx, rateControlMode };
  const [usePaintOverQuality, setUsePaintOverQuality] = useConditionalSetting(
    USE_PAINT_OVER_QUALITY_SPEC, serverSettings, paintOverCtx, [serverSettings, rateControlMode], readPaintOverStored);
  const [videoFullColor, setVideoFullColor] = useConditionalSetting(
    VIDEO_FULLCOLOR_SPEC, serverSettings, conditionalCtx, [serverSettings]);
  const [use_cpu, setUseCpu] = useConditionalSetting(
    USE_CPU_SPEC, serverSettings, conditionalCtx, [serverSettings]);
  const [videoStreamingMode, setVideoStreamingMode] = useConditionalSetting(
    VIDEO_STREAMING_MODE_SPEC, serverSettings, conditionalCtx, [serverSettings]);
  const [forceAlignedResolution, setForceAlignedResolution] = useConditionalSetting(
    FORCE_ALIGNED_RESOLUTION_SPEC, serverSettings, conditionalCtx, [serverSettings]);
  const [use_browser_cursors, setUseBrowserCursors] = useConditionalSetting(
    USE_BROWSER_CURSORS_SPEC, serverSettings, conditionalCtx, [serverSettings]);
  /**
   * The cursor value the core reports as in effect (multi-monitor forces
   * browser cursors on), `null` until reported; displayed over the stored
   * preference so the toggle cannot lie about the live state.
   */
  const [effectiveCursor, setEffectiveCursor] = useState(null);
  const [antiAliasing, setAntiAliasing] = useState(() => {
    const saved = localStorage.getItem(getPrefixedKey("antiAliasingEnabled"));
    return saved !== null ? saved === "true" : true;
  });
  const [enableBinaryClipboard, setEnableBinaryClipboard] = useState(() => {
    const saved = localStorage.getItem(getPrefixedKey("enable_binary_clipboard"));
    return saved !== null ? saved === 'true' : DEFAULT_ENABLE_BINARY_CLIPBOARD;
  });
  const [presetValue, setPresetValue] = useState("");
  const [clientFps, setClientFps] = useState(0);
  const [audioLevel, setAudioLevel] = useState(0);
  const audioMeterRef = useRef(null);
  const [bandwidthMbps, setBandwidthMbps] = useState(0);
  const [latencyMs, setLatencyMs] = useState(0);
  const [cpuPercent, setCpuPercent] = useState(0);
  const [gpuPercent, setGpuPercent] = useState(0);
  const [sysMemPercent, setSysMemPercent] = useState(0);
  const [gpuMemPercent, setGpuMemPercent] = useState(0);
  const [sysMemUsed, setSysMemUsed] = useState(null);
  const [sysMemTotal, setSysMemTotal] = useState(null);
  const [gpuMemUsed, setGpuMemUsed] = useState(null);
  const [gpuMemTotal, setGpuMemTotal] = useState(null);
  const [hoveredItem, setHoveredItem] = useState(null);
  const [tooltipPosition, setTooltipPosition] = useState({ x: 0, y: 0 });
  const [isVideoActive, setIsVideoActive] = useState(true);
  const [isAudioActive, setIsAudioActive] = useState(true);
  const [isMicrophoneActive, setIsMicrophoneActive] = useState(false);
  const [isWebcamActive, setIsWebcamActive] = useState(false);
  const [isGamepadEnabled, setIsGamepadEnabled] = useState(true);
  const [dashboardClipboardContent, setDashboardClipboardContent] =
    useState("");
  /**
   * Large server clipboards arrive as a bounded, truncated preview; editing
   * it would echo the cut-down text back over the real server clipboard on
   * blur, so truncated content renders read-only.
   */
  const [dashboardClipboardTruncated, setDashboardClipboardTruncated] =
    useState(false);
  const [audioInputDevices, setAudioInputDevices] = useState([]);
  const [audioOutputDevices, setAudioOutputDevices] = useState([]);
  const [selectedInputDeviceId, setSelectedInputDeviceId] = useState("default");
  const [selectedOutputDeviceId, setSelectedOutputDeviceId] =
    useState("default");
  const [isOutputSelectionSupported, setIsOutputSelectionSupported] =
    useState(false);
  const [audioDeviceError, setAudioDeviceError] = useState(null);
  const [isLoadingAudioDevices, setIsLoadingAudioDevices] = useState(false);
  const [gamepadStates, setGamepadStates] = useState({});
  const [hasReceivedGamepadData, setHasReceivedGamepadData] = useState(false);
  const [sectionsOpen, setSectionsOpen] = useState({
    settings: false,
    audioSettings: false,
    screenSettings: false,
    stats: false,
    clipboard: false,
    // A phone lands on gamepads: the reason to open the dashboard on touch at all.
    gamepads: IS_MOBILE_CLIENT,
    files: false,
    apps: false,
    sharing: false,
    shortcuts: false,
  });
  const [notifications, setNotifications] = useState([]);
  const notificationTimeouts = useRef({});
  const [isFilesModalOpen, setIsFilesModalOpen] = useState(false);
  const [isAppsModalOpen, setIsAppsModalOpen] = useState(false);
  const [keyboardButtonPosition, setKeyboardButtonPosition] = useState({ bottom: 20, right: 20 });
  const dragInfo = useRef({
    isDragging: false,
    hasDragged: false,
    pointerId: null,
    startX: 0,
    startY: 0,
    initialBottom: 0,
    initialRight: 0,
  });
  /**
   * Toggle handle position as a percentage of the viewport height, so a
   * resize keeps it proportional; persisted across reloads.
   */
  const [toggleHandleTopPct, setToggleHandleTopPct] = useState(() => {
    const stored = parseFloat(localStorage.getItem(getPrefixedKey("sidebarToggleTopPct")));
    return Number.isFinite(stored) ? clampToggleHandleTopPct(stored) : 50;
  });
  const toggleDragInfo = useRef({
    isDragging: false,
    hasDragged: false,
    pointerId: null,
    startX: 0,
    startY: 0,
    initialTopPct: 50,
    lastTopPct: 50,
  });
  const isWebrtc = streamMode === STREAM_MODE_WEBRTC;
  /**
   * Filters an encoder list to what this transport can play: on the
   * WebSocket transport only encoders this engine can decode (JPEG alone
   * without WebCodecs). The static lists seed the picker; the server's own
   * allowed list replaces them as soon as settings arrive.
   */
  const offeredEncoders = useCallback(
    (list) => (isWebrtc ? list : decodableEncoders(list)), [isWebrtc]);
  const [dynamicEncoderOptions, setDynamicEncoderOptions] = useState(
    () => offeredEncoders(isWebrtc ? encoderOptionsWR : encoderOptions));
  /** Audio bitrate stops the slider indexes into: the server's allowed enum, else the local list. */
  const audioBitrateChoices = (serverSettings?.audio_bitrate?.allowed?.map((v) => parseInt(v, 10))) || audioBitrateOptions;

  const DEBOUNCE_DELAY = 500;

  /** One debounced poster for the component's lifetime, so a burst of slider moves coalesces into a single post. */
  const debouncedPostSetting = useMemo(
    () => debounce((setting) => {
      window.postMessage(
        { type: "settings", settings: setting },
        window.location.origin
      );
    }, DEBOUNCE_DELAY),
    []
  );

  /** The two push channels a spec can propagate through; each spec decides which. */
  const conditionalIo = {
    postSetting: (obj) => debouncedPostSetting(obj),
    postToCore: (obj) => window.postMessage(obj, window.location.origin),
  };
  /**
   * Uniform write path for conditional settings: optimistic setState, an
   * optional persist (explicit choices pin, derived ones do not), and
   * propagation through the spec.
   * @param {object} spec
   * @param {any} uiValue The value as the UI holds it.
   * @param {Function} setValue The setting's state setter.
   * @param {{persist?: boolean}} [opts]
   */
  const writeConditional = (spec, uiValue, setValue, opts = {}) => {
    setValue(uiValue);
    if (opts.persist) {
      localStorage.setItem(getPrefixedKey(spec.storageKey),
        spec.serialize ? spec.serialize(uiValue) : String(uiValue));
      localStorage.setItem(explicitChoiceKey(spec), "true");
    }
    spec.propagate(spec.toServer ? spec.toServer(uiValue) : uiValue, conditionalCtx, conditionalIo);
  };

  /**
   * Seeds every plain setting from the server's payload, with a stored value
   * taking precedence when the server allows it; the conditional settings
   * sync through their `useConditionalSetting` hooks instead. This writes
   * state instead of deriving during render because every value stays
   * editable afterwards; recomputing each render would discard the user's
   * edits. `scaling_dpi` resolves an operator override (which the server
   * refuses to let clients clobber), then the stored pick, then the derived
   * local-display default, the order the cores send on every connect
   * independent of the resolution mode; a derived value that differs from the
   * server's is posted.
   */
  /* eslint-disable react-hooks/set-state-in-effect */
  useEffect(() => {
    if (!serverSettings) return;
    const getStoredInt = (key) => parseInt(localStorage.getItem(getPrefixedKey(key)), 10);
    const getStoredBool = (key, fallback = false) => {
      const stored = localStorage.getItem(getPrefixedKey(key));
      return stored !== null ? stored === 'true' : fallback;
    };
    const s_encoder = serverSettings.encoder;
    if (s_encoder) {
      const allowed = offeredEncoders(s_encoder.allowed);
      const stored = localStorage.getItem(getPrefixedKey("encoder"));
      const final = allowed.includes(stored) ? stored
        : (allowed.includes(s_encoder.value) || allowed.length === 0) ? s_encoder.value : allowed[0];
      setEncoder(final);
      setDynamicEncoderOptions(allowed);
    }
    const s_framerate = serverSettings.framerate;
    if (s_framerate) {
      const stored = getStoredInt("framerate");
      const final = !isNaN(stored)
        ? Math.max(s_framerate.min, Math.min(s_framerate.max, stored))
        : s_framerate.default;
      setFramerate(final);
    }
    const s_video_bitrate = serverSettings.video_bitrate;
    if (s_video_bitrate) {
      const stored = parseInt(localStorage.getItem(getPrefixedKey("video_bitrate")), 10);
      const final = !isNaN(stored)
        ? Math.max(s_video_bitrate.min, Math.min(s_video_bitrate.max, stored))
        : s_video_bitrate.default;
      setVideoBitrate(final);
    }
    const s_audio_bitrate = serverSettings.audio_bitrate;
    if (s_audio_bitrate) {
      const stored = getStoredInt("audio_bitrate");
      // allowed holds strings; compare as string and keep the result numeric.
      let final = s_audio_bitrate.allowed?.includes(String(stored)) ? stored : parseInt(s_audio_bitrate.value, 10);
      // A NaN would persist and break the slider; the server's highest allowed value is the fallback.
      if (Number.isNaN(final)) {
        const allowed = s_audio_bitrate.allowed;
        const maxAllowed = parseInt(allowed?.[allowed.length - 1], 10);
        final = Number.isNaN(maxAllowed) ? 320000 : maxAllowed;
      }
      setAudioBitrate(final);
    }
    const s_video_crf = serverSettings.video_crf;
    if (s_video_crf) {
      const stored = getStoredInt("video_crf");
      const final = !isNaN(stored)
        ? Math.max(s_video_crf.min, Math.min(s_video_crf.max, stored))
        : s_video_crf.default;
      setVideoCRF(final);
    }
    const s_jpeg_quality = serverSettings.jpeg_quality;
    if (s_jpeg_quality) {
      const stored = getStoredInt("jpeg_quality");
      const final = !isNaN(stored)
        ? Math.max(s_jpeg_quality.min, Math.min(s_jpeg_quality.max, stored))
        : s_jpeg_quality.default;
      setJpegQuality(final);
    }
    const s_paint_over_jpeg_quality = serverSettings.paint_over_jpeg_quality;
    if (s_paint_over_jpeg_quality) {
      const stored = getStoredInt("paint_over_jpeg_quality");
      const final = !isNaN(stored)
        ? Math.max(s_paint_over_jpeg_quality.min, Math.min(s_paint_over_jpeg_quality.max, stored))
        : s_paint_over_jpeg_quality.default;
      setPaintOverJpegQuality(final);
    }
    const s_video_paintover_crf = serverSettings.video_paintover_crf;
    if (s_video_paintover_crf) {
      const stored = getStoredInt("video_paintover_crf");
      const final = !isNaN(stored)
        ? Math.max(s_video_paintover_crf.min, Math.min(s_video_paintover_crf.max, stored))
        : s_video_paintover_crf.default;
      setVideoPaintoverCRF(final);
    }
    const s_video_paintover_burst = serverSettings.video_paintover_burst_frames;
    if (s_video_paintover_burst) {
      const stored = getStoredInt("video_paintover_burst_frames");
      const final = !isNaN(stored)
        ? Math.max(s_video_paintover_burst.min, Math.min(s_video_paintover_burst.max, stored))
        : s_video_paintover_burst.default;
      setVideoPaintoverBurstFrames(final);
    }
    const s_scaling_dpi = serverSettings.scaling_dpi;
    if (s_scaling_dpi) {
      const stored = getStoredInt("scaling_dpi");
      const storedAllowed = s_scaling_dpi.allowed.includes(String(stored));
      const serverVal = parseInt(s_scaling_dpi.value, 10);
      const derived = deriveDpiFromDpr();
      const willPostDerived = !storedAllowed && !s_scaling_dpi.overridden
        && derived !== serverVal;
      const final = s_scaling_dpi.overridden ? serverVal
        : storedAllowed ? stored
        : derived;
      setSelectedDpi(final);
      if (willPostDerived) {
        debouncedPostSetting({ scaling_dpi: derived });
      }
    }
    const s_enable_binary_clipboard = serverSettings.enable_binary_clipboard;
    if (s_enable_binary_clipboard) {
      const final = s_enable_binary_clipboard.locked ? s_enable_binary_clipboard.value : getStoredBool("enable_binary_clipboard", s_enable_binary_clipboard.value);
      setEnableBinaryClipboard(final);
    }
    const s_ui_title = serverSettings.ui_title;
    if (s_ui_title) {
        setUiTitle(s_ui_title.value);
    }
    const s_ui_show_logo = serverSettings.ui_show_logo;
    if (s_ui_show_logo) {
        setUiShowLogo(s_ui_show_logo.value);
    }
  }, [serverSettings, debouncedPostSetting, offeredEncoders]);
  /* eslint-enable react-hooks/set-state-in-effect */

  /**
   * Rate control: the hook only sets local state, so when the resolved
   * default diverges from what the server is applying (a transport switch
   * seeds the session with the previous mode's value) this pushes it so the
   * encoder follows. Pinned, locked or operator-overridden values resolve to
   * the server's value and post nothing. The core persists every mode it is
   * told to apply and resends it on the next connect, so without an explicit
   * pick the stored value is an echo, not a choice: it is dropped once it
   * stops matching what the ladder resolves, or it would outlive the
   * derivation, and an operator override with it.
   */
  useEffect(() => {
    if (!serverSettings) return;
    if (serverSettings.enable_rate_control?.value === false) return;
    const rcKey = RATE_CONTROL_SPEC.storageKey;
    const resolved = resolveSpec(
      RATE_CONTROL_SPEC, serverSettings, conditionalCtx, readRateControlStored);
    if (!isExplicitChoice(RATE_CONTROL_SPEC)
      && readStored(rcKey) !== null && readStored(rcKey) !== resolved) {
      localStorage.removeItem(getPrefixedKey(rcKey));
    }
    if (isSettingPinned(RATE_CONTROL_SPEC, serverSettings, readRateControlStored)) return;
    const serverValue = serverSettings[RATE_CONTROL_SPEC.serverKey]?.value;
    if (resolved && serverValue !== undefined && resolved !== serverValue) {
      writeConditional(RATE_CONTROL_SPEC, resolved, setRateControlMode, { persist: false });
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [serverSettings]);

  /**
   * HiDPI's stored value has the same stale echo rate control has: the core
   * persists every applied `useCssScaling`, so a derived pick outlives the
   * resolution mode that produced it unless dropped when the ladder moves on.
   */
  useEffect(() => {
    if (!serverSettings) return;
    const key = HIDPI_SPEC.storageKey;
    const resolved = resolveSpec(
      HIDPI_SPEC, serverSettings, conditionalCtx, readHidpiStored);
    if (!isExplicitChoice(HIDPI_SPEC)
      && readStored(key) !== null
      && readStored(key) !== HIDPI_SPEC.serialize(resolved)) {
      localStorage.removeItem(getPrefixedKey(key));
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [serverSettings]);

  /**
   * Paint-over: pushes the default the resolved rate control implies so the
   * encoder agrees, and drops the unmarked stored echo once the ladder moves
   * on, the same shape as the rate-control derivation.
   */
  useEffect(() => {
    if (!serverSettings) return;
    const key = USE_PAINT_OVER_QUALITY_SPEC.storageKey;
    const resolved = resolveSpec(
      USE_PAINT_OVER_QUALITY_SPEC, serverSettings, { ...conditionalCtx, rateControlMode }, readPaintOverStored);
    if (!isExplicitChoice(USE_PAINT_OVER_QUALITY_SPEC)
      && readStored(key) !== null
      && readStored(key) !== String(resolved)) {
      localStorage.removeItem(getPrefixedKey(key));
    }
    if (isSettingPinned(USE_PAINT_OVER_QUALITY_SPEC, serverSettings, readPaintOverStored)) return;
    const serverValue = serverSettings.use_paint_over_quality?.value;
    if (resolved !== undefined && serverValue !== undefined && resolved !== serverValue) {
      writeConditional(USE_PAINT_OVER_QUALITY_SPEC, resolved, setUsePaintOverQuality, { persist: false });
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [serverSettings, rateControlMode]);

  /** UI scaling pick: persisted, so it pins across reloads and stops the startup derived-default post. */
  const handleDpiScalingChange = (event) => {
    const newDpi = parseInt(event.target.value, 10);
    setSelectedDpi(newDpi);
    localStorage.setItem(getPrefixedKey("scaling_dpi"), newDpi.toString());
    debouncedPostSetting({ scaling_dpi: newDpi });
  };

  const DRAG_THRESHOLD = 10;

  const handlePointerDown = (e) => {
    dragInfo.current.isDragging = true;
    dragInfo.current.hasDragged = false;
    dragInfo.current.pointerId = e.pointerId;
    dragInfo.current.startX = e.clientX;
    dragInfo.current.startY = e.clientY;
    dragInfo.current.initialBottom = keyboardButtonPosition.bottom;
    dragInfo.current.initialRight = keyboardButtonPosition.right;
    e.currentTarget.setPointerCapture(e.pointerId);
  };

  const handlePointerMove = (e) => {
    if (!dragInfo.current.isDragging) return;

    const dx = e.clientX - dragInfo.current.startX;
    const dy = e.clientY - dragInfo.current.startY;

    if (!dragInfo.current.hasDragged && (Math.abs(dx) > DRAG_THRESHOLD || Math.abs(dy) > DRAG_THRESHOLD)) {
      dragInfo.current.hasDragged = true;
    }

    if (dragInfo.current.hasDragged) {
      setKeyboardButtonPosition({
        bottom: dragInfo.current.initialBottom - dy,
        right: dragInfo.current.initialRight - dx,
      });
    }
  };

  const handlePointerUp = (e) => {
    if (e.currentTarget.hasPointerCapture(dragInfo.current.pointerId)) {
      e.currentTarget.releasePointerCapture(e.pointerId);
    }
    dragInfo.current.isDragging = false;
    dragInfo.current.pointerId = null;
  };

  const onKeyboardButtonClick = (e) => {
    if (dragInfo.current.hasDragged) {
      e.preventDefault();
      e.stopPropagation();
      dragInfo.current.hasDragged = false;
      return;
    }
    handleShowVirtualKeyboard();
  };

  const handleTogglePointerDown = (e) => {
    toggleDragInfo.current.isDragging = true;
    toggleDragInfo.current.hasDragged = false;
    toggleDragInfo.current.pointerId = e.pointerId;
    toggleDragInfo.current.startX = e.clientX;
    toggleDragInfo.current.startY = e.clientY;
    toggleDragInfo.current.initialTopPct = toggleHandleTopPct;
    toggleDragInfo.current.lastTopPct = toggleHandleTopPct;
    e.currentTarget.setPointerCapture(e.pointerId);
  };

  const handleTogglePointerMove = (e) => {
    if (!toggleDragInfo.current.isDragging) return;

    const dx = e.clientX - toggleDragInfo.current.startX;
    const dy = e.clientY - toggleDragInfo.current.startY;

    if (!toggleDragInfo.current.hasDragged && (Math.abs(dx) > DRAG_THRESHOLD || Math.abs(dy) > DRAG_THRESHOLD)) {
      toggleDragInfo.current.hasDragged = true;
    }

    if (toggleDragInfo.current.hasDragged) {
      const vh = window.innerHeight || 1;
      const newTopPct = clampToggleHandleTopPct(
        toggleDragInfo.current.initialTopPct + (dy / vh) * 100
      );
      toggleDragInfo.current.lastTopPct = newTopPct;
      setToggleHandleTopPct(newTopPct);
    }
  };

  /**
   * Ends a handle drag and persists its position. `pointerId` is null when
   * the pointerup had no matching pointerdown (capture lost), and
   * `hasPointerCapture(null)` coerces to id 0 and could release a foreign
   * capture, so capture is only released for the tracked pointer.
   */
  const handleTogglePointerUp = (e) => {
    const pid = toggleDragInfo.current.pointerId;
    if (pid !== null && e.currentTarget.hasPointerCapture(pid)) {
      e.currentTarget.releasePointerCapture(pid);
    }
    const didDrag = toggleDragInfo.current.hasDragged;
    const finalPct = toggleDragInfo.current.lastTopPct;
    toggleDragInfo.current.isDragging = false;
    toggleDragInfo.current.pointerId = null;
    if (didDrag) {
      try {
        localStorage.setItem(getPrefixedKey("sidebarToggleTopPct"), finalPct.toString());
      } catch {
        // A blocked/full storage write only loses persistence for this drag.
      }
    }
  };

  const onToggleHandleClick = (e) => {
    if (toggleDragInfo.current.hasDragged) {
      e.preventDefault();
      e.stopPropagation();
      toggleDragInfo.current.hasDragged = false;
      return;
    }
    toggleSidebar();
  };

  const toggleAppsModal = () => setIsAppsModalOpen(!isAppsModalOpen);
  const toggleFilesModal = () => setIsFilesModalOpen(!isFilesModalOpen);
  /**
   * Pops the on-screen keyboard by focusing the core's `#keyboard-input-assist`
   * input; the next touch on the stream overlay blurs it again.
   */
  const handleShowVirtualKeyboard = useCallback(() => {
    console.log("Dashboard: Directly handling virtual keyboard pop.");
    const kbdAssistInput = document.getElementById('keyboard-input-assist');
    const mainInteractionOverlay = document.getElementById('overlayInput');
    if (kbdAssistInput) {
      kbdAssistInput.removeAttribute('aria-hidden');
      kbdAssistInput.value = '';
      kbdAssistInput.focus();
      console.log("Focused #keyboard-input-assist element to pop keyboard.");
      if (mainInteractionOverlay) {
        mainInteractionOverlay.addEventListener(
          "touchstart",
          () => {
            if (document.activeElement === kbdAssistInput) {
              kbdAssistInput.blur();
              console.log("Blurred #keyboard-input-assist on main overlay touch.");
              kbdAssistInput.setAttribute('aria-hidden', 'true');
            }
          }, {
            once: true,
            passive: true
          }
        );
      } else {
         console.warn("Could not find #overlayInput to attach blur listener.");
      }
    } else {
      console.error("Could not find #keyboard-input-assist element to focus.");
    }
  }, []);

  /**
   * Lists the audio devices for the audio section, taking a momentary
   * microphone permission so labels are readable. Output routing goes through
   * whatever the active core plays on: the WebRTC core's `<video>` element
   * (`HTMLMediaElement.setSinkId`) or the WebSocket core's AudioContext
   * (`AudioContext.setSinkId`, which Firefox lacks), so the one in use is
   * probed or the output picker would render and do nothing. The selections
   * are left alone: the core keeps the devices this dashboard picked for the
   * life of the page, so a reopened section shows them rather than defaulting.
   */
  const populateAudioDevices = useCallback(async () => {
    console.log("Dashboard: Attempting to populate audio devices...");
    setIsLoadingAudioDevices(true);
    setAudioDeviceError(null);
    setAudioInputDevices([]);
    setAudioOutputDevices([]);
    const supportsSinkId = isWebrtc
      ? "setSinkId" in HTMLMediaElement.prototype
      : typeof AudioContext !== "undefined" && "setSinkId" in AudioContext.prototype;
    setIsOutputSelectionSupported(supportsSinkId);
    console.log(
      "Dashboard: Output device selection supported:",
      supportsSinkId
    );
    try {
      console.log(
        "Dashboard: Requesting temporary microphone permission for device listing..."
      );
      const tempStream = await navigator.mediaDevices.getUserMedia({
        audio: true,
      });
      tempStream.getTracks().forEach((track) => track.stop());
      console.log("Dashboard: Temporary permission granted/available.");
      console.log("Dashboard: Enumerating media devices...");
      const devices = await navigator.mediaDevices.enumerateDevices();
      console.log("Dashboard: Devices found:", devices);
      const inputs = [];
      const outputs = [];
      devices.forEach((device, index) => {
        if (!device.deviceId) {
          console.warn(
            "Dashboard: Skipping device with missing deviceId:",
            device
          );
          return;
        }
        const label =
          device.label ||
          (device.kind === "audioinput"
            ? t("sections.audio.defaultInputLabelFallback", {
                index: index + 1,
              })
            : t("sections.audio.defaultOutputLabelFallback", {
                index: index + 1,
              }));
        if (device.kind === "audioinput") {
          inputs.push({ deviceId: device.deviceId, label: label });
        } else if (device.kind === "audiooutput" && supportsSinkId) {
          outputs.push({ deviceId: device.deviceId, label: label });
        }
      });
      setAudioInputDevices(inputs);
      setAudioOutputDevices(outputs);
      console.log(
        `Dashboard: Populated ${inputs.length} inputs, ${outputs.length} outputs.`
      );
    } catch (err) {
      console.error(
        "Dashboard: Error getting media devices or permissions:",
        err
      );
      let userMessageKey = "sections.audio.deviceErrorDefault";
      let errorVars = { errorName: err.name || "Unknown error" };
      if (err.name === "NotAllowedError")
        userMessageKey = "sections.audio.deviceErrorPermission";
      else if (err.name === "NotFoundError")
        userMessageKey = "sections.audio.deviceErrorNotFound";
      setAudioDeviceError(t(userMessageKey, errorVars));
    } finally {
      setIsLoadingAudioDevices(false);
    }
  }, [t, isWebrtc]);

  /** Folds or unfolds a section; opening the audio section enumerates devices. */
  const toggleSection = useCallback(
    (sectionKey) => {
      const isOpening = !sectionsOpen[sectionKey];
      setSectionsOpen((prev) => ({ ...prev, [sectionKey]: !prev[sectionKey] }));
      if (sectionKey === "audioSettings" && isOpening) {
        populateAudioDevices();
      }
    },
    [sectionsOpen, populateAudioDevices]
  );
  const baseUrl = typeof window !== 'undefined' ? window.location.href.split('#')[0] : '';
  const sharingLinks = [
    {
      id: "shared",
      label: t("sections.sharing.viewerLabel"),
      tooltip: t("sections.sharing.viewerTooltip"),
      hash: "#shared",
    },
    ...[2, 3, 4].map((n) => ({
      id: `player${n}`,
      label: t("sections.sharing.controllerLabel", { n }),
      tooltip: t("sections.sharing.controllerTooltip", { n }),
      hash: `#player${n}`,
    })),
  ];
  /** Copies a sharing link and reports the outcome as a notification. */
  const handleCopyLink = async (textToCopy, label) => {
    if (!navigator.clipboard) {
      console.warn("Clipboard API not available.");
      return;
    }
    try {
      await navigator.clipboard.writeText(textToCopy);
      const id = `copy-success-${label.toLowerCase().replace(/\s+/g, '-')}`;
      setNotifications(prev => {
        const filtered = prev.filter(n => n.id !== id);
        const newNotifs = [...filtered, {
          id,
          fileName: t("notifications.copiedTitle", { label: label }),
          status: 'end',
          message: t("notifications.copiedMessage", { textToCopy: textToCopy }),
          timestamp: Date.now(),
          fadingOut: false,
        }];
        return newNotifs.slice(-MAX_NOTIFICATIONS);
      });
      scheduleNotificationRemoval(id, NOTIFICATION_TIMEOUT_SUCCESS);
    } catch (err) {
      console.error("Failed to copy link: ", err);
      const id = `copy-error-${label.toLowerCase().replace(/\s+/g, '-')}`;
      setNotifications(prev => {
        const filtered = prev.filter(n => n.id !== id);
        const newNotifs = [...filtered, {
          id,
          fileName: t("notifications.copyFailedTitle", { label: label }),
          status: 'error',
          message: t('notifications.copyFailedError'),
          timestamp: Date.now(),
          fadingOut: false,
        }];
        return newNotifs.slice(-MAX_NOTIFICATIONS);
      });
      scheduleNotificationRemoval(id, NOTIFICATION_TIMEOUT_ERROR);
    }
  };
  /**
   * Re-derives rate control after an encoder or software-encoding change.
   * Rate control follows those unless pinned by an explicit client or server
   * choice, and a derived change is not persisted, so it keeps following.
   * @param {object} ctxOverrides The value just chosen, ahead of the re-render that would put it in `conditionalCtx`.
   */
  const rederiveRateControl = (ctxOverrides) => {
    if (!rateControlEnabled
      || isSettingPinned(RATE_CONTROL_SPEC, serverSettings, readRateControlStored)) return;
    const rcResolved = resolveSpec(
      RATE_CONTROL_SPEC, serverSettings,
      { ...conditionalCtx, ...ctxOverrides }, readRateControlStored);
    if (rcResolved !== rateControlMode) {
      writeConditional(RATE_CONTROL_SPEC, rcResolved, setRateControlMode, { persist: false });
    }
  };
  /**
   * Encoder pick, one knob for both transports; the server switches the
   * pipeline encoder on it. The choice is persisted immediately so
   * `conditionalCtx.activeEncoder`, which reads localStorage, does not lag
   * during the post debounce and let a `serverSettings` sync re-derive rate
   * control off the stale encoder.
   */
  const handleEncoderChange = (event) => {
    const selectedEncoder = event.target.value;
    setEncoder(selectedEncoder);
    localStorage.setItem(getPrefixedKey("encoder"), selectedEncoder);
    debouncedPostSetting({ encoder: selectedEncoder });
    rederiveRateControl({ activeEncoder: selectedEncoder });
  };
  const handleWebcamEncoderChange = (event) => {
    const preference = event.target.value;
    setWebcamEncoderChoice(preference);
    localStorage.setItem(getPrefixedKey("webcam_encoder"), preference);
    debouncedPostSetting({ webcam_encoder: preference });
  };
  // Derived, not synced: the server default stands in for a missing choice,
  // locked overrides it.
  const wceServer = serverSettings?.webcam_encoder;
  const wceServerValue = webcamEncoderOptions.includes(wceServer?.value) ? wceServer.value : null;
  const wceChoice = webcamEncoderOptions.includes(webcamEncoderChoice) ? webcamEncoderChoice : null;
  const webcamEncoder = (wceServer?.locked && wceServerValue) || wceChoice || wceServerValue || "auto";
  const handleFramerateChange = (event) => {
    const selectedFramerate = parseInt(event.target.value, 10);
    setFramerate(selectedFramerate);
    debouncedPostSetting({ framerate: selectedFramerate });
  };
  /** Video bitrate slider: its value is an index into `videoBitrateOptions`. */
  const handleVideoBitrateChange = (event) => {
    const index = parseInt(event.target.value, 10);
    const selectedVideoBitrate = videoBitrateOptions[index];
    if (selectedVideoBitrate === undefined) return;
    setVideoBitrate(selectedVideoBitrate)
    debouncedPostSetting({ video_bitrate: selectedVideoBitrate})
  };
  const handleAudioBitrateChange = (selectedAudioBitrate) => {
    if (Number.isNaN(selectedAudioBitrate)) selectedAudioBitrate = DEFAULT_AUDIO_BITRATE;
    setAudioBitrate(selectedAudioBitrate)
    debouncedPostSetting({ audio_bitrate: selectedAudioBitrate})
  }
  const handleJpegQualityChange = (event) => {
    const selectedQuality = parseInt(event.target.value, 10);
    setJpegQuality(selectedQuality);
    debouncedPostSetting({ jpeg_quality: selectedQuality });
  };
  const handlePaintOverJpegQualityChange = (event) => {
    const selectedQuality = parseInt(event.target.value, 10);
    setPaintOverJpegQuality(selectedQuality);
    debouncedPostSetting({ paint_over_jpeg_quality: selectedQuality });
  };
  const handleVideoCRFChange = (event) => {
    const selectedCRF = parseInt(event.target.value, 10);
    setVideoCRF(selectedCRF);
    debouncedPostSetting({ video_crf: selectedCRF });
  };
  const handleH264PaintoverCRFChange = (event) => {
    const selectedCRF = parseInt(event.target.value, 10);
    setVideoPaintoverCRF(selectedCRF);
    debouncedPostSetting({ video_paintover_crf: selectedCRF });
  };
  const handleH264PaintoverBurstChange = (event) => {
    const selectedFrames = parseInt(event.target.value, 10);
    setVideoPaintoverBurstFrames(selectedFrames);
    debouncedPostSetting({ video_paintover_burst_frames: selectedFrames });
  };
  const handleH264FullColorToggle = () => {
    writeConditional(VIDEO_FULLCOLOR_SPEC, !videoFullColor, setVideoFullColor, { persist: true });
  };
  const handleUsePaintOverQualityToggle = () => {
    writeConditional(USE_PAINT_OVER_QUALITY_SPEC, !usePaintOverQuality, setUsePaintOverQuality, { persist: true });
  };
  const handleUseCpuToggle = () => {
    writeConditional(USE_CPU_SPEC, !use_cpu, setUseCpu, { persist: true });
    rederiveRateControl({ useCpu: !use_cpu });
  };
  const handleH264StreamingModeToggle = () => {
    writeConditional(VIDEO_STREAMING_MODE_SPEC, !videoStreamingMode, setVideoStreamingMode, { persist: true });
  };
  /** Rate control pick: an explicit choice, persisted so encoder changes stop overriding it. */
  const handleRateControlChange = (event) => {
    writeConditional(RATE_CONTROL_SPEC, event.target.value, setRateControlMode, { persist: true });
  };
  const handleAudioInputChange = (event) => {
    const deviceId = event.target.value;
    setSelectedInputDeviceId(deviceId);
    window.postMessage(
      { type: "audioDeviceSelected", context: "input", deviceId: deviceId },
      window.location.origin
    );
  };
  const handleAudioOutputChange = (event) => {
    const deviceId = event.target.value;
    setSelectedOutputDeviceId(deviceId);
    window.postMessage(
      { type: "audioDeviceSelected", context: "output", deviceId: deviceId },
      window.location.origin
    );
  };
  const handlePresetChange = (event) => {
    const selectedValue = event.target.value;
    setPresetValue(selectedValue);
    if (!selectedValue) return;
    const parts = selectedValue.split("x");
    if (parts.length === 2) {
      const width = parseInt(parts[0], 10),
        height = parseInt(parts[1], 10);
      if (!isNaN(width) && width > 0 && !isNaN(height) && height > 0) {
        const evenWidth = roundDownToEven(width),
          evenHeight = roundDownToEven(height);
        setManualWidth(evenWidth.toString());
        setManualHeight(evenHeight.toString());
        localStorage.setItem(getPrefixedKey("manual_width"), evenWidth.toString());
        localStorage.setItem(getPrefixedKey("manual_height"), evenHeight.toString());
        window.postMessage(
          { type: "setManualResolution", width: evenWidth, height: evenHeight },
          window.location.origin
        );
        deriveHidpiForResolution(true);
      } else
        console.error(
          "Dashboard: Error parsing selected resolution preset:",
          selectedValue
        );
    }
  };
  /**
   * A half-typed size stays in component state: the stored `manual_width`
   * and `manual_height` mean "a manual resolution is applied", which the
   * HiDPI and UI-scaling derivations read, so only Set, a preset, or Reset
   * may write them.
   */
  const handleManualWidthChange = (event) => {
    setManualWidth(event.target.value);
    setPresetValue("");
  };
  const handleManualHeightChange = (event) => {
    setManualHeight(event.target.value);
    setPresetValue("");
  };
  const handleScaleLocallyToggle = () => {
    const newState = !scaleLocally;
    setScaleLocally(newState);
    window.postMessage(
      { type: "setScaleLocally", value: newState },
      window.location.origin
    );
  };
  /** HiDPI toggle: an explicit choice, pinned; the core persists `useCssScaling` when it applies the message. */
  const handleHidpiToggle = () => {
    writeConditional(HIDPI_SPEC, !hidpiEnabled, setHidpiEnabled, { persist: true });
  };
  /**
   * Manual and preset resolutions pair with CSS scaling: HiDPI off when one
   * is set, on when reset, as a derived (unpinned) write through the uniform
   * path. An explicit toggle or a locked or overridden server value pins
   * HiDPI and stops the resolution buttons from re-deriving it.
   * @param {boolean} manual Whether a manual resolution is now applied.
   */
  const deriveHidpiForResolution = (manual) => {
    if (isSettingPinned(HIDPI_SPEC, serverSettings, readHidpiStored)) return;
    writeConditional(HIDPI_SPEC, !manual, setHidpiEnabled, { persist: false });
  };
  /**
   * Reset-to-window also returns UI scaling to its derived
   * (`devicePixelRatio`) default: the pinned client choice is dropped so the
   * derived default governs again, and the value propagates like a user
   * change. Locked or operator-explicit (overridden) values govern scaling
   * instead, the same gate as the startup derived-default post.
   */
  const resetDpiToDerivedDefault = () => {
    const s = serverSettings?.scaling_dpi;
    if (s?.locked || s?.overridden) return;
    localStorage.removeItem(getPrefixedKey("scaling_dpi"));
    const derived = deriveDpiFromDpr();
    setSelectedDpi(derived);
    debouncedPostSetting({ scaling_dpi: derived });
  };
  /**
   * Reset-to-window also restores HiDPI to its default. Unlike the
   * resolution-derived writes, which respect a pinned choice, a reset means
   * "back to defaults", so the client's own pin is dropped even under an
   * operator-explicit value: `use_css_scaling`'s overridden does not imply
   * locked, and a kept pin would keep outranking the operator's value in the
   * resolution ladder. The operator value when explicit, else the derived
   * default, is then applied without storing; only a locked value leaves
   * everything alone.
   */
  const resetHidpiToDerivedDefault = () => {
    const s = serverSettings?.use_css_scaling;
    if (s?.locked) return;
    localStorage.removeItem(getPrefixedKey(HIDPI_SPEC.storageKey));
    localStorage.removeItem(explicitChoiceKey(HIDPI_SPEC));
    const uiValue = s?.overridden ? s.value !== true : true;
    writeConditional(HIDPI_SPEC, uiValue, setHidpiEnabled, { persist: false });
  };
  const handleForceAlignedResolutionToggle = () => {
    writeConditional(FORCE_ALIGNED_RESOLUTION_SPEC, !forceAlignedResolution, setForceAlignedResolution, { persist: true });
  };
  const handleAntiAliasingToggle = () => {
    const newState = !antiAliasing;
    setAntiAliasing(newState);
    window.postMessage(
      { type: "setAntiAliasing", value: newState },
      window.location.origin
    );
  };
  /**
   * Browser cursors toggle. The core owns persistence: the new preference is
   * propagated and the core reports the effective value back. The next value
   * derives from the displayed one: while multi-monitor forces the toggle on
   * the base preference may be off, and negating the base would silently
   * persist the forced value over the user's real choice.
   */
  const handleUseBrowserCursorsToggle = () => {
    writeConditional(USE_BROWSER_CURSORS_SPEC, !(effectiveCursor ?? use_browser_cursors), setUseBrowserCursors, { persist: false });
  };
  const handleEnableBinaryClipboardToggle = () => {
    const newState = !enableBinaryClipboard;
    setEnableBinaryClipboard(newState);
    debouncedPostSetting({ enable_binary_clipboard: newState });
  };
  const handleSetManualResolution = () => {
    const width = parseInt(manual_width.trim(), 10),
      height = parseInt(manual_height.trim(), 10);
    if (isNaN(width) || width <= 0 || isNaN(height) || height <= 0) {
      alert(t("alerts.invalidResolution"));
      return;
    }
    const evenWidth = roundDownToEven(width),
      evenHeight = roundDownToEven(height);
    setManualWidth(evenWidth.toString());
    setManualHeight(evenHeight.toString());
    setPresetValue("");
    localStorage.setItem(getPrefixedKey("manual_width"), evenWidth.toString());
    localStorage.setItem(getPrefixedKey("manual_height"), evenHeight.toString());
    window.postMessage(
      { type: "setManualResolution", width: evenWidth, height: evenHeight },
      window.location.origin
    );
    deriveHidpiForResolution(true);
  };
  const handleResetResolution = () => {
    setManualWidth("");
    setManualHeight("");
    setPresetValue("");
    localStorage.removeItem(getPrefixedKey("manual_width"));
    localStorage.removeItem(getPrefixedKey("manual_height"));
    window.postMessage(
      { type: "resetResolutionToWindow" },
      window.location.origin
    );
    resetHidpiToDerivedDefault();
    resetDpiToDerivedDefault();
  };
  const handleVideoToggle = () =>
    window.postMessage(
      { type: "pipelineControl", pipeline: "video", enabled: !isVideoActive },
      window.location.origin
    );
  const handleAudioToggle = () =>
    window.postMessage(
      { type: "pipelineControl", pipeline: "audio", enabled: !isAudioActive },
      window.location.origin
    );
  const handleMicrophoneToggle = () =>
    window.postMessage(
      {
        type: "pipelineControl",
        pipeline: "microphone",
        enabled: !isMicrophoneActive,
      },
      window.location.origin
    );
  const handleWebcamToggle = () =>
    window.postMessage(
      {
        type: "pipelineControl",
        pipeline: "webcam",
        enabled: !isWebcamActive,
      },
      window.location.origin
    );
  const handleGamepadToggle = () =>
    window.postMessage(
      { type: "gamepadControl", enabled: !isGamepadEnabled },
      window.location.origin
    );
  /**
   * Leaves fullscreen through whichever prefixed API exists. Entering is
   * handed to the core, which owns what each mode locks; exiting is the
   * browser's own call either way.
   */
  const exitFullscreen = () => {
    const exit = document.exitFullscreen || document.mozCancelFullScreen
      || document.webkitExitFullscreen || document.msExitFullscreen;
    if (exit) Promise.resolve(exit.call(document)).catch(() => {});
  };
  const handleGamingModeRequest = () => {
    if (document.fullscreenElement) exitFullscreen();
    else window.postMessage({ type: "requestGamingMode" }, window.location.origin);
  };
  const handleBrowserFullscreen = () => {
    if (document.fullscreenElement) exitFullscreen();
    else window.postMessage({ type: "requestFullscreen" }, window.location.origin);
  };
  const handleClipboardChange = (event) =>
    setDashboardClipboardContent(event.target.value);
  const clipboardImageInputRef = useRef(null);
  /**
   * Hands a picked image to the core's `clipboardImageUpdate` path (a File
   * is a Blob), which sends it through the binary clipboard exactly like a
   * focus-synced local clipboard image; any other file raises a warning
   * notification. Same contract as the wish dashboard.
   */
  const handleClipboardImageUpload = (event) => {
    const file = event.target.files && event.target.files[0];
    if (file && file.type.startsWith("image/")) {
      window.postMessage(
        { type: "clipboardImageUpdate", imageBlob: file },
        window.location.origin
      );
    } else if (file) {
      window.postMessage(
        {
          type: "fileUpload",
          payload: {
            status: "warning",
            fileName: "clipboard-image",
            message: t("notifications.clipboardImageRejected", {
              name: file.name,
              mime: file.type || "unknown",
            }),
          },
        },
        window.location.origin
      );
    }
    // Cleared so the same file can be picked again.
    event.target.value = "";
  };
  /** Pushes the edited clipboard text to the server on blur, never a truncated preview. */
  const handleClipboardBlur = (event) => {
    if (dashboardClipboardTruncated) return;
    window.postMessage(
      { type: "clipboardUpdateFromUI", text: event.target.value },
      window.location.origin
    );
  };
  const toggleTheme = () => {
    const newTheme = theme === "dark" ? "light" : "dark";
    setTheme(newTheme);
    localStorage.setItem("theme", newTheme);
  };
  /**
   * Switches the transport through `/api/switch`, then posts `mode` so the
   * core reloads into it. `window.__selkiesModeSwitching` is set before the
   * request because the server tears down the old peer (WebSocket close code
   * 4000) before it responds, and without the flag the active core would
   * surface a spurious "Server disconnected" alert. The endpoint is gated on
   * the master token (Bearer) when set, or Basic credentials via same-origin;
   * with Basic Auth off the dashboard is not given the token, so a 401 prompts
   * for it once, keeps it in sessionStorage, and retries; a token the server
   * rejects is dropped so the next attempt re-prompts. A failed switch clears
   * the flag again, since no reload follows and a kept flag would hide a real
   * disconnect.
   */
  const handleStreamModeChange = async (event) => {
    const newMode = event.target.value;
    console.log("Change of stream mode requested:", newMode);
    window.__selkiesModeSwitching = true;
    try {
      const MASTER_TOKEN_KEY = "selkies_master_token";
      const doSwitch = () => {
        const headers = { "Content-Type": "application/json" };
        let storedToken = null;
        try { storedToken = sessionStorage.getItem(MASTER_TOKEN_KEY); } catch { /* sessionStorage unavailable */ }
        if (storedToken) headers["Authorization"] = `Bearer ${storedToken}`;
        return fetch(`${getRoutePrefix()}/api/switch`, {
          method: "POST",
          headers,
          credentials: "same-origin",
          body: JSON.stringify({ mode: newMode }),
        });
      };
      let response = await doSwitch();
      if (response.status === 401) {
        const entered = (typeof window !== "undefined" && window.prompt)
          ? window.prompt("Switching the stream mode requires the Selkies master token:")
          : null;
        if (entered && entered.trim()) {
          try { sessionStorage.setItem(MASTER_TOKEN_KEY, entered.trim()); } catch { /* sessionStorage unavailable */ }
          response = await doSwitch();
        }
      }

      if (!response.ok) {
        if (response.status === 401) { try { sessionStorage.removeItem(MASTER_TOKEN_KEY); } catch { /* sessionStorage unavailable */ } }
        throw new Error(`Request failed with status ${response.status}`);
      }
      await response.json();
      setStreamMode(newMode);
      window.postMessage(
        { type: "mode", mode: newMode },
        window.location.origin
      );
    } catch (error) {
        window.__selkiesModeSwitching = false;
        console.error("Error switching stream mode:", error);
    }
  }
  const handleMouseEnter = (e, itemKey) => {
    setHoveredItem(itemKey);
    setTooltipPosition({ x: e.clientX + 10, y: e.clientY + 10 });
  };
  const handleMouseLeave = () => setHoveredItem(null);

  /** Touch gamepad toggle: `TOUCH_GAMEPAD_SETUP` on first activation, `TOUCH_GAMEPAD_VISIBILITY` afterwards. */
  const handleToggleTouchGamepad = useCallback(() => {
    const newActiveState = !isTouchGamepadActive;
    setIsTouchGamepadActive(newActiveState);

    if (newActiveState && !isTouchGamepadSetup) {
      window.postMessage(
        {
          type: "TOUCH_GAMEPAD_SETUP",
          payload: { targetDivId: TOUCH_GAMEPAD_HOST_DIV_ID, visible: true },
        },
        window.location.origin
      );
      setIsTouchGamepadSetup(true);
      console.log(
        "Dashboard: Touch Gamepad SETUP sent, targetDivId:",
        TOUCH_GAMEPAD_HOST_DIV_ID,
        "visible: true"
      );
    } else if (isTouchGamepadSetup) {
      window.postMessage(
        {
          type: "TOUCH_GAMEPAD_VISIBILITY",
          payload: {
            visible: newActiveState,
            targetDivId: TOUCH_GAMEPAD_HOST_DIV_ID,
          },
        },
        window.location.origin
      );
      console.log(
        `Dashboard: Touch Gamepad VISIBILITY sent, targetDivId:`,
        TOUCH_GAMEPAD_HOST_DIV_ID,
        `visible: ${newActiveState}`
      );
    }
  }, [isTouchGamepadActive, isTouchGamepadSetup]);

  const handleToggleTrackpadMode = useCallback(() => {
    const newActiveState = !isTrackpadModeActive;
    setIsTrackpadModeActive(newActiveState);
    const message = newActiveState ? "touchinput:trackpad" : "touchinput:touch";
    console.log(`Dashboard: Toggling trackpad mode. Sending: ${message}`);
    window.postMessage({ type: message }, window.location.origin);
  }, [isTrackpadModeActive]);

  /** Tooltip text for a stats gauge. */
  const getTooltipContent = useCallback(
    (itemKey) => {
      const memNA = t("sections.stats.tooltipMemoryNA");
      switch (itemKey) {
        case "cpu":
          return t("sections.stats.tooltipCpu", {
            value: cpuPercent.toFixed(1),
          });
        case "gpu":
          return t("sections.stats.tooltipGpu", {
            value: gpuPercent.toFixed(1),
          });
        case "sysmem": {
          const fu =
            sysMemUsed !== null ? formatBytes(sysMemUsed, 2, raw) : memNA;
          const ft =
            sysMemTotal !== null ? formatBytes(sysMemTotal, 2, raw) : memNA;
          return fu !== memNA && ft !== memNA
            ? t("sections.stats.tooltipSysMem", { used: fu, total: ft })
            : `${t("sections.stats.sysMemLabel")}: ${memNA}`;
        }
        case "gpumem": {
          const gu =
            gpuMemUsed !== null ? formatBytes(gpuMemUsed, 2, raw) : memNA;
          const gt =
            gpuMemTotal !== null ? formatBytes(gpuMemTotal, 2, raw) : memNA;
          return gu !== memNA && gt !== memNA
            ? t("sections.stats.tooltipGpuMem", { used: gu, total: gt })
            : `${t("sections.stats.gpuMemLabel")}: ${memNA}`;
        }
        case "fps":
          return t("sections.stats.tooltipFps", { value: clientFps });
        case "audio":
          return t("sections.stats.tooltipAudioLevel", { value: audioLevel });
        case "bandwidth":
          return t("sections.stats.tooltipBandwidth", { value: bandwidthMbps.toFixed(2) }, `Bandwidth: ${bandwidthMbps.toFixed(2)} Mbps`);
        case "latency":
          return t("sections.stats.tooltipLatency", { value: latencyMs.toFixed(1) }, `Latency: ${latencyMs.toFixed(1)} ms`);
        default:
          return "";
      }
    },
    [
      t,
      raw,
      cpuPercent,
      gpuPercent,
      sysMemUsed,
      sysMemTotal,
      gpuMemUsed,
      gpuMemTotal,
      clientFps,
      audioLevel,
      bandwidthMbps,
      latencyMs,
    ]
  );

  const removeNotification = useCallback((id) => {
    setNotifications((prev) => prev.filter((n) => n.id !== id));
    if (notificationTimeouts.current[id]) {
      clearTimeout(notificationTimeouts.current[id].fadeTimer);
      clearTimeout(notificationTimeouts.current[id].removeTimer);
      delete notificationTimeouts.current[id];
    }
  }, []);

  /** Fades a notification out shortly before `delay` and removes it at `delay`. */
  const scheduleNotificationRemoval = useCallback(
    (id, delay) => {
      if (notificationTimeouts.current[id]) {
        clearTimeout(notificationTimeouts.current[id].fadeTimer);
        clearTimeout(notificationTimeouts.current[id].removeTimer);
      }
      const fadeTimer = setTimeout(
        () =>
          setNotifications((prev) =>
            prev.map((n) => (n.id === id ? { ...n, fadingOut: true } : n))
          ),
        delay - NOTIFICATION_FADE_DURATION
      );
      const removeTimer = setTimeout(() => removeNotification(id), delay);
      notificationTimeouts.current[id] = { fadeTimer, removeTimer };
    },
    [removeNotification]
  );

  const handleUploadClick = () =>
    window.dispatchEvent(new CustomEvent("requestFileUpload"));

  /**
   * Polls the core's `window` stats into the gauges. The stats only render
   * inside the open sidebar, so the fifteen or so setState calls, each a full
   * Sidebar re-render, are skipped while it is closed or the tab is hidden.
   */
  useEffect(() => {
    const readStats = () => {
      if (!isOpen || document.hidden) return;
      const cs = window.system_stats,
        su = cs?.mem_used ?? null,
        st = cs?.mem_total ?? null;
      setCpuPercent(cs?.cpu_percent ?? 0);
      setSysMemUsed(su);
      setSysMemTotal(st);
      setSysMemPercent(
        su !== null && st !== null && st > 0 ? (su / st) * 100 : 0
      );
      const cgs = window.gpu_stats,
        gp = cgs?.gpu_percent ?? cgs?.utilization_gpu ?? 0;
      setGpuPercent(gp);
      const gu =
        cgs?.mem_used ?? cgs?.memory_used ?? cgs?.used_gpu_memory_bytes ?? null;
      const gt =
        cgs?.mem_total ??
        cgs?.memory_total ??
        cgs?.total_gpu_memory_bytes ??
        null;
      setGpuMemUsed(gu);
      setGpuMemTotal(gt);
      setGpuMemPercent(
        gu !== null && gt !== null && gt > 0 ? (gu / gt) * 100 : 0
      );
      setClientFps(window.fps ?? 0);
      // x141 is the websockets worklet's own scale (RMS x141, a full-scale sine
      // reads 100), applied to the WebRTC analyser's raw RMS so both match.
      const coreLevel = window.currentAudioLevel;
      const level = typeof coreLevel === "number"
        ? coreLevel
        : (readStreamAudioLevel(audioMeterRef) ?? 0) * 141;
      setAudioLevel(Math.min(100, Math.round(level)));
      const netStats = window.network_stats;
      setBandwidthMbps(netStats?.bandwidth_mbps ?? 0);
      setLatencyMs(netStats?.latency_ms ?? 0);
    };
    const intervalId = setInterval(readStats, STATS_READ_INTERVAL_MS);
    return () => clearInterval(intervalId);
  }, [isOpen]);

  /** The core message listener; the module docblock lists the message types handled. */
  useEffect(() => {
    const handleWindowMessage = (event) => {
      if (event.origin !== window.location.origin) return;
      const message = event.data;
      if (typeof message === "object" && message !== null) {
        if (message.type === "pipelineStatusUpdate") {
          if (message.video !== undefined) setIsVideoActive(message.video);
          if (message.audio !== undefined) setIsAudioActive(message.audio);
          if (message.microphone !== undefined)
            setIsMicrophoneActive(message.microphone);
          if (message.webcam !== undefined)
            setIsWebcamActive(message.webcam);
        } else if (message.type === "effectiveCursorState" && typeof message.value === "boolean") {
          setEffectiveCursor(message.value);
        } else if (message.type === 'clientRoleUpdate') {
          const viewer = message.role === 'viewer';
          setIsViewerRole(viewer);
          // A viewer gets no sidebar: the handle goes, and one opened before the demotion folds.
          if (viewer) {
            setIsToggleVisible(false);
            setIsOpen(false);
          }
        } else if (message.type === "toggleDashboard") {
          if (!isViewerRole) setIsOpen((prev) => !prev);
        } else if (message.type === "toggleTouchGamepad") {
          handleToggleTouchGamepad();
        } else if (message.type === "gamepadControl") {
          if (message.enabled !== undefined)
            setIsGamepadEnabled(message.enabled);
        } else if (message.type === "sidebarButtonStatusUpdate") {
          if (message.video !== undefined) setIsVideoActive(message.video);
          if (message.audio !== undefined) setIsAudioActive(message.audio);
          if (message.microphone !== undefined)
            setIsMicrophoneActive(message.microphone);
          if (message.webcam !== undefined)
            setIsWebcamActive(message.webcam);
          if (message.gamepad !== undefined)
            setIsGamepadEnabled(message.gamepad);
        } else if (message.type === "clipboardContentUpdate") {
          if (typeof message.text === "string") {
            setDashboardClipboardContent(message.text);
            setDashboardClipboardTruncated(message.truncated === true);
          }
        } else if (message.type === "audioDeviceSelected") {
          if (message.deviceId) {
            if (message.context === "input")
              setSelectedInputDeviceId(message.deviceId);
            else if (message.context === "output")
              setSelectedOutputDeviceId(message.deviceId);
          }
        } else if (
          message.type === "gamepadButtonUpdate" ||
          message.type === "gamepadAxisUpdate"
        ) {
          if (!hasReceivedGamepadData) setHasReceivedGamepadData(true);
          const gpIndex = message.gamepadIndex;
          if (gpIndex === undefined || gpIndex === null) return;
          setGamepadStates((prev) => {
            const ns = { ...prev };
            if (!ns[gpIndex]) ns[gpIndex] = { buttons: {}, axes: {} };
            else
              ns[gpIndex] = {
                buttons: { ...(ns[gpIndex].buttons || {}) },
                axes: { ...(ns[gpIndex].axes || {}) },
              };
            if (message.type === "gamepadButtonUpdate")
              ns[gpIndex].buttons[message.buttonIndex] = message.value || 0;
            else
              ns[gpIndex].axes[message.axisIndex] = Math.max(
                -1,
                Math.min(1, message.value || 0)
              );
            return ns;
          });
        } else if (message.type === "fileUpload") {
          const {
            status,
            fileName,
            progress,
            fileSize,
            message: errMsg,
            code,
          } = message.payload;
          // Settles the pending optimistic apps update first; a stale launch
          // match is lifecycle noise, not a notice.
          if (code === "commandFailed" && !resolveFailedAppCommand(errMsg))
            return;
          // An unknown or absent code keeps the raw message for third-party
          // consumers: the translator answers a miss with the key itself.
          const codeKey =
            typeof code === "string" && (code.startsWith("clipboard") || code === "commandFailed")
              ? `notifications.${code}`
              : null;
          const codeMsg = codeKey ? t(codeKey, { detail: errMsg }) : null;
          const localizedMsg = codeMsg && codeMsg !== codeKey ? codeMsg : errMsg;
          const id = fileName;
          setNotifications((prev) => {
            const exIdx = prev.findIndex((n) => n.id === id);
            if (exIdx === -1) {
              if (prev.length < MAX_NOTIFICATIONS && status === "start")
                return [
                  ...prev,
                  {
                    id,
                    fileName,
                    status: "progress",
                    progress: 0,
                    fileSize,
                    message: null,
                    timestamp: Date.now(),
                    fadingOut: false,
                  },
                ];
              if (prev.length < MAX_NOTIFICATIONS && status === "warning") {
                scheduleNotificationRemoval(id, NOTIFICATION_TIMEOUT_SUCCESS);
                return [
                  ...prev,
                  {
                    id,
                    fileName: t("notifications.warningPrefix"),
                    status: "warn",
                    message: localizedMsg,
                    timestamp: Date.now(),
                    fadingOut: false,
                  }
                ];
              } else return prev;
            } else if (exIdx !== -1) {
              const un = [...prev],
                cn = un[exIdx];
              if (notificationTimeouts.current[id]) {
                clearTimeout(notificationTimeouts.current[id].fadeTimer);
                clearTimeout(notificationTimeouts.current[id].removeTimer);
                delete notificationTimeouts.current[id];
              }
              if (status === "progress")
                un[exIdx] = {
                  ...cn,
                  status: "progress",
                  progress,
                  timestamp: Date.now(),
                  fadingOut: false,
                };
              else if (status === "end") {
                un[exIdx] = {
                  ...cn,
                  status: "end",
                  progress: 100,
                  message: null,
                  timestamp: Date.now(),
                  fadingOut: false,
                };
                scheduleNotificationRemoval(id, NOTIFICATION_TIMEOUT_SUCCESS);
              } else if (status === "error") {
                const text = errMsg
                  ? `${t("notifications.errorPrefix")} ${errMsg}`
                  : t("notifications.unknownError");
                un[exIdx] = {
                  ...cn,
                  status: "error",
                  progress: 100,
                  message: text,
                  timestamp: Date.now(),
                  fadingOut: false,
                };
                scheduleNotificationRemoval(id, NOTIFICATION_TIMEOUT_ERROR);
              } else if (status === "warning") {
                  un[exIdx] = {
                    ...cn,
                    fileName: t("notifications.warningPrefix"),
                    status: "warn",
                    message: localizedMsg,
                    timestamp: Date.now(),
                    fadingOut: false,
                  };
                  scheduleNotificationRemoval(id, NOTIFICATION_TIMEOUT_ERROR);
              }
              return un;
            } else return prev;
          });
        } else if (message.type === "serverSettings") {
            const encoders = message.payload?.encoder?.allowed
            if (encoders && Array.isArray(encoders)) {
              const newEncoderOptions = offeredEncoders(
                Array.isArray(encoders) && encoders.length > 0
                  ? encoders
                  : (isWebrtc? encoderOptionsWR: encoderOptions));
              setDynamicEncoderOptions(newEncoderOptions);
          }
        } else if (message.type === "trackpadModeUpdate") {
          if (typeof message.enabled === 'boolean') {
            setIsTrackpadModeActive(message.enabled);
          }
        }
      }
    };
    window.addEventListener("message", handleWindowMessage);
    return () => {
      window.removeEventListener("message", handleWindowMessage);
      Object.values(notificationTimeouts.current).forEach((timers) => {
        clearTimeout(timers.fadeTimer);
        clearTimeout(timers.removeTimer);
      });
      notificationTimeouts.current = {};
    };
  }, [
    hasReceivedGamepadData,
    scheduleNotificationRemoval,
    removeNotification,
    handleToggleTouchGamepad,
    t,
    dynamicEncoderOptions,
    isOpen,
    isWebrtc,
    offeredEncoders,
    isViewerRole,
  ]);

  const gaugeSize = 80,
    gaugeStrokeWidth = 8,
    gaugeRadius = gaugeSize / 2 - gaugeStrokeWidth / 2;
  const gaugeCircumference = 2 * Math.PI * gaugeRadius,
    gaugeCenter = gaugeSize / 2;
  const cpuOffset = calculateGaugeOffset(
    cpuPercent,
    gaugeRadius,
    gaugeCircumference
  );
  const gpuOffset = calculateGaugeOffset(
    gpuPercent,
    gaugeRadius,
    gaugeCircumference
  );
  const sysMemOffset = calculateGaugeOffset(
    sysMemPercent,
    gaugeRadius,
    gaugeCircumference
  );
  const gpuMemOffset = calculateGaugeOffset(
    gpuMemPercent,
    gaugeRadius,
    gaugeCircumference
  );
  const fpsPercent = Math.min(
    100,
    (clientFps / (framerate || DEFAULT_FRAMERATE)) * 100
  );
  const fpsOffset = calculateGaugeOffset(
    fpsPercent,
    gaugeRadius,
    gaugeCircumference
  );
  const audioLevelOffset = calculateGaugeOffset(
    audioLevel,
    gaugeRadius,
    gaugeCircumference
  );
  /**
   * Full scale of the bandwidth gauge: the traffic the session is configured
   * for (video target in kbps plus audio in bps), not an arbitrary link speed.
   */
  const maxBandwidthMbps = Math.max(0.1, videoBitrate / 1000 + audioBitrate / 1_000_000);
  const MAX_LATENCY_MS = 1000;
  const bandwidthPercent = Math.min(100, (bandwidthMbps / maxBandwidthMbps) * 100);
  const bandwidthOffset = calculateGaugeOffset(
    bandwidthPercent,
    gaugeRadius,
    gaugeCircumference
  );
  const latencyPercent = Math.min(100, (latencyMs / MAX_LATENCY_MS) * 100);
  const latencyOffset = calculateGaugeOffset(
    latencyPercent,
    gaugeRadius,
    gaugeCircumference
  );
  const translatedCommonResolutions = commonResolutionValues.map(
    (value, index) => ({
      value: value,
      text:
        index === 0
          ? t("sections.screen.resolutionPresetSelect")
          : raw?.resolutionPresets?.[value] || value,
    })
  );

  /** One encoder knob serves both transports; CBR/CRF applies to every H.264 encoder on both. */
  const activeEncoder = encoder;
  const H264_ENCODERS = ["h264enc", "h264enc-striped", "nvh264enc"];
  const showFPS = [
    "jpeg",
    "h264enc-striped",
    "h264enc",
  ].includes(encoder);
  const showCRF = H264_ENCODERS.includes(activeEncoder);
  const showH264Options = H264_ENCODERS.includes(activeEncoder);
  const showJpegOptions = encoder === 'jpeg';
  const showPaintOverQualityToggle = showH264Options || showJpegOptions;
  /**
   * The mode the encoder is actually using, which picks the quality slider:
   * the server's when rate control is disabled.
   */
  const appliedRateControlMode = rateControlEnabled
    ? rateControlMode
    : (serverSettings?.rate_control_mode?.value ?? rateControlMode);

  /**
   * CBR slider stops within the server's range: the sub-Mbps steps, 1000-kbps
   * steps to 100000, then the coarse steps to 1000000.
   */
  const videoBitrateOptions = (() => {
    const min = serverSettings?.video_bitrate?.min ?? 100;
    const max = serverSettings?.video_bitrate?.max ?? 1000000;
    const stops = SUB_MBPS_BITRATE_STEPS.filter((v) => v >= min && v <= max);
    for (let v = Math.max(1000, Math.ceil(min / 1000) * 1000); v <= Math.min(100000, Math.floor(max / 1000) * 1000); v += 1000) stops.push(v);
    stops.push(...COARSE_MBPS_BITRATE_STEPS.filter((v) => v >= min && v <= max));
    return stops.length ? stops : [min];
  })();
  const bitrateSliderIndex = (() => {
    const exact = videoBitrateOptions.indexOf(videoBitrate);
    if (exact >= 0) return exact;
    const above = videoBitrateOptions.findIndex((v) => v >= videoBitrate);
    return above >= 0 ? above : videoBitrateOptions.length - 1;
  })();
  const formatBitrate = (v) => `${v / 1000} Mbps`;
  if (serverSettings && serverSettings.ui_show_sidebar?.value === false) {
    return null;
  }
  const sidebarClasses = `sidebar ${isOpen ? "is-open" : ""} theme-${theme}`;
  const showCoreButtons = !isSecondaryDisplay && (renderableSettings.coreButtons ?? true);
  // Touch-only tiles sharing the action row: they wrap below the core five at
  // the same size instead of forming a differently sized bar of their own.
  const showKeyboardTile =
    (isMobile || hasDetectedTouch) && (renderableSettings.softButtons ?? true);
  const showTrackpadTile =
    (isMobile || hasDetectedTouch) && (renderableSettings.trackpad ?? true);
  const filteredSharingLinks = sharingLinks.filter(link => {
    if (link.id === 'shared') return renderableSettings.enableShared ?? true;
    if (link.id === 'player2') return renderableSettings.enablePlayer2 ?? true;
    if (link.id === 'player3') return renderableSettings.enablePlayer3 ?? true;
    if (link.id === 'player4') return renderableSettings.enablePlayer4 ?? true;
    return false;
  });

  return (
    <>
      {isToggleVisible && !isGamingMode && (
        <div
          className='toggle-handle'
          onClick={onToggleHandleClick}
          onPointerDown={handleTogglePointerDown}
          onPointerMove={handleTogglePointerMove}
          onPointerUp={handleTogglePointerUp}
          onPointerCancel={handleTogglePointerUp}
          style={{ top: `${toggleHandleTopPct}%`, touchAction: 'none' }}
          title={isOpen ? t("closeDashboardTitle") : t("openDashboardTitle")}
        >
          <div className="toggle-indicator"></div>
        </div>
      )}
      {availablePlacements && (() => {
        const arrowBaseStyle = {
          position: 'absolute',
          width: '100px',
          height: '100px',
          backgroundColor: 'rgba(97, 218, 251, 0.8)',
          color: 'var(--sidebar-bg, #20232a)',
          border: '2px solid var(--sidebar-bg, #20232a)',
          borderRadius: '15px',
          fontSize: '48px',
          display: 'flex',
          justifyContent: 'center',
          alignItems: 'center',
          cursor: 'pointer',
          pointerEvents: 'all',
          boxShadow: '0 4px 15px rgba(0, 0, 0, 0.3)',
          transition: 'transform 0.2s ease',
        };

        const handleArrowClick = (e, direction, screen) => {
          e.stopPropagation();
          launchWindow(direction, screen);
        };

        return (
          <div 
            style={{
              position: 'fixed',
              top: 0,
              left: 0,
              width: '100vw',
              height: '100vh',
              zIndex: 9999,
              pointerEvents: 'auto'
            }}
            onClick={() => setAvailablePlacements(null)}
          >
            {availablePlacements.up && (
              <button style={{...arrowBaseStyle, top: '40px', left: '50%', transform: 'translateX(-50%)'}} onClick={(e) => handleArrowClick(e, 'up', availablePlacements.up)}>▲</button>
            )}
            {availablePlacements.down && (
              <button style={{...arrowBaseStyle, bottom: '40px', left: '50%', transform: 'translateX(-50%)'}} onClick={(e) => handleArrowClick(e, 'down', availablePlacements.down)}>▼</button>
            )}
            {availablePlacements.left && (
              <button style={{...arrowBaseStyle, left: '40px', top: '50%', transform: 'translateY(-50%)'}} onClick={(e) => handleArrowClick(e, 'left', availablePlacements.left)}>◄</button>
            )}
            {availablePlacements.right && (
              <button style={{...arrowBaseStyle, right: '40px', top: '50%', transform: 'translateY(-50%)'}} onClick={(e) => handleArrowClick(e, 'right', availablePlacements.right)}>►</button>
            )}
          </div>
        );
      })()}
      <div className={sidebarClasses}>
          <div className="sidebar-header">
            {uiShowLogo && (
              <a
                href="https://github.com/selkies-project/selkies"
                target="_blank"
                rel="noopener noreferrer"
              >
                <SelkiesLogo width={30} height={30} t={t} />
              </a>
            )}
            <a
              href="https://github.com/selkies-project/selkies"
              target="_blank"
              rel="noopener noreferrer"
            >
              <h2>{uiTitle}</h2>
            </a>
            <div className="header-controls">
            <div
              className={`theme-toggle ${theme}`}
              onClick={toggleTheme}
              title={t("toggleThemeTitle")}
            >
              <svg className="icon moon-icon" viewBox="0 0 24 24">
                <path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z"></path>
              </svg>
              <svg className="icon sun-icon" viewBox="0 0 24 24">
                <circle cx="12" cy="12" r="5"></circle>
                <line x1="12" y1="1" x2="12" y2="3"></line>
                <line x1="12" y1="21" x2="12" y2="23"></line>
                <line x1="4.22" y1="4.22" x2="5.64" y2="5.64"></line>
                <line x1="18.36" y1="18.36" x2="19.78" y2="19.78"></line>
                <line x1="1" y1="12" x2="3" y2="12"></line>
                <line x1="21" y1="12" x2="23" y2="12"></line>
                <line x1="4.22" y1="19.78" x2="5.64" y2="18.36"></line>
                <line x1="18.36" y1="5.64" x2="19.78" y2="4.22"></line>
              </svg>
            </div>
            {(renderableSettings.fullscreen ?? true) && (
              <button
                className="header-action-button fullscreen-button"
                onClick={handleBrowserFullscreen}
                title={t("fullscreenTitle")}
              >
                <FullscreenIcon />
              </button>
            )}
            {(renderableSettings.gamingMode ?? true) && (
              <button
                className="header-action-button gaming-mode-button"
                onClick={handleGamingModeRequest}
                title={`${t("gamingModeTitle", "Gaming Mode")} \u2014 ${t("gamingModeHint", "Fullscreen with the pointer and keyboard locked")}`}
              >
                <GamingModeIcon />
              </button>
            )}
          </div>
        </div>

        {(showCoreButtons || showKeyboardTile || showTrackpadTile) && (
          <div className="sidebar-action-buttons">
            {showCoreButtons && (renderableSettings.videoToggle ?? true) && (
              <button
                className={`action-button ${isVideoActive ? "active" : ""}`}
                onClick={handleVideoToggle}
                title={t(
                  isVideoActive
                    ? "buttons.videoStreamDisableTitle"
                    : "buttons.videoStreamEnableTitle"
                )}
              >
                <ScreenIcon />
              </button>
            )}
            {showCoreButtons && (renderableSettings.audioToggle ?? true) && (
              <button
                className={`action-button ${isAudioActive ? "active" : ""}`}
                onClick={handleAudioToggle}
                title={t(
                  isAudioActive
                    ? "buttons.audioStreamDisableTitle"
                    : "buttons.audioStreamEnableTitle"
                )}
              >
                <SpeakerIcon />
              </button>
            )}
            {showCoreButtons && (renderableSettings.microphoneToggle ?? true) && (
              <button
                className={`action-button ${isMicrophoneActive ? "active" : ""}`}
                onClick={handleMicrophoneToggle}
                title={t(
                  isMicrophoneActive
                    ? "buttons.microphoneDisableTitle"
                    : "buttons.microphoneEnableTitle"
                )}
              >
                <MicrophoneIcon />
              </button>
            )}
            {showCoreButtons && (renderableSettings.webcamToggle ?? true) && (
              <button
                className={`action-button ${isWebcamActive ? "active" : ""}`}
                onClick={handleWebcamToggle}
                title={t(
                  isWebcamActive
                    ? "buttons.webcamDisableTitle"
                    : "buttons.webcamEnableTitle"
                )}
              >
                <WebcamIcon />
              </button>
            )}
            {showCoreButtons && (renderableSettings.gamepadToggle ?? true) && (
              <button
                className={`action-button ${isGamepadEnabled ? "active" : ""}`}
                onClick={handleGamepadToggle}
                title={t(
                  isGamepadEnabled
                    ? "buttons.gamepadDisableTitle"
                    : "buttons.gamepadEnableTitle"
                )}
              >
                <GamepadIcon />
              </button>
            )}
            {showKeyboardTile && (
              <button
                className={`action-button keyboard-toggle-button ${isKeyboardButtonVisible ? "active" : ""}`}
                onClick={toggleKeyboardButtonVisibility}
                title={t("keyboardButtonToggleTitle", "Keyboard Button")}
              >
                <KeyboardIcon />
              </button>
            )}
            {showTrackpadTile && (
              <button
                className={`action-button trackpad-mode-button ${isTrackpadModeActive ? "active" : ""}`}
                onClick={handleToggleTrackpadMode}
                title={t("trackpadModeTitle", "Trackpad Mode")}
              >
                <TrackpadIcon />
              </button>
            )}
          </div>
        )}
        
        {(isMobile || hasDetectedTouch) && (renderableSettings.softButtons ?? true) && (
            <div className="sidebar-mobile-key-actions">
              <button
                className={`mobile-key-button ${heldKeys.Control ? "active" : ""}`}
                onClick={() => handleHoldKeyClick('Control', 'ControlLeft')}
                onMouseDown={(e) => e.preventDefault()}
              >
                CTL
              </button>
              <button
                className={`mobile-key-button ${heldKeys.Alt ? "active" : ""}`}
                onClick={() => handleHoldKeyClick('Alt', 'AltLeft')}
                onMouseDown={(e) => e.preventDefault()}
              >
                ALT
              </button>
              <button
                className={`mobile-key-button ${heldKeys.Meta ? "active" : ""}`}
                onClick={() => handleHoldKeyClick('Meta', 'MetaLeft')}
                onMouseDown={(e) => e.preventDefault()}
              >
                WIN
              </button>
              <button
                className="mobile-key-button"
                onClick={() => handleOnceKeyClick('Tab', 'Tab')}
                onMouseDown={(e) => e.preventDefault()}
              >
                TAB
              </button>
              <button
                className="mobile-key-button"
                onClick={() => handleOnceKeyClick('Escape', 'Escape')}
                onMouseDown={(e) => e.preventDefault()}
              >
                ESC
              </button>
            </div>
        )}

        {/* Viewers can't apply stream settings (the server ignores their
            SETTINGS payloads); hide the posting sections instead of rendering
            controls that silently do nothing. */}
        {!isViewerRole && (renderableSettings.videoSettings ?? true) && (
          <div className="sidebar-section">
            <div
              className="sidebar-section-header"
              onClick={() => toggleSection("settings")}
              role="button"
              aria-expanded={sectionsOpen.settings}
              aria-controls="settings-content"
              tabIndex="0"
              onKeyDown={(e) =>
                (e.key === "Enter" || e.key === " ") && toggleSection("settings")
              }
            >
              <h3>{t("sections.video.title")}</h3>
              <span className="section-toggle-icon">
                {sectionsOpen.settings ? <CaretUpIcon /> : <CaretDownIcon />}
              </span>
            </div>
            {sectionsOpen.settings && (
                <div className="sidebar-section-content" id="settings-content">
                  {((renderableSettings.enableDualMode ?? window.__SELKIES_DUAL_MODE__) ?? false) && !isViewerRole && (
                    <div className="dev-setting-item">
                      {" "}
                      <label htmlFor="streamModeSelect">
                        {t("streamingModeTitle", "Streaming Mode")}
                      </label>{" "}
                      <select
                        id="streamModeSelect"
                        value={streamMode}
                        onChange={handleStreamModeChange}
                      >
                        {" "}
                        {STREAMING_MODES.map((mode) => (
                          <option key={mode} value={mode}>
                            {displayLabel(mode)}
                          </option>
                        ))}{" "}
                      </select>{" "}
                    </div>
                  )}
                {(renderableSettings.encoder ?? true) && (
                  <div className="dev-setting-item">
                    <label htmlFor="encoderSelect">
                      {t("sections.video.encoderLabel")}
                    </label>
                    <select
                      id="encoderSelect"
                      value={encoder}
                      onChange={handleEncoderChange}
                      disabled={!serverSettings || dynamicEncoderOptions.length <= 1}
                    >
                      {dynamicEncoderOptions.map((enc) => (
                        <option key={enc} value={enc}>
                          {displayLabel(enc)}
                        </option>
                      ))}
                    </select>
                  </div>
                )}
                {!isWebrtc && (renderableSettings.webcamEncoder ?? true) && (
                  <div className="dev-setting-item">
                    <label htmlFor="webcamEncoderSelect">
                      {t("sections.video.webcamEncoderLabel")}
                    </label>
                    <select
                      id="webcamEncoderSelect"
                      value={webcamEncoder}
                      onChange={handleWebcamEncoderChange}
                      disabled={!serverSettings || !!serverSettings.webcam_encoder?.locked}
                    >
                      {webcamEncoderOptions.map((pref) => (
                        <option key={pref} value={pref}>
                          {displayLabel(pref)}
                        </option>
                      ))}
                    </select>
                  </div>
                )}
                {rateControlEnabled && showH264Options && (
                  <div className="dev-setting-item">
                    <label htmlFor="rateControlSelect">
                      {t("sections.video.rateControlLabel")}
                    </label>
                    <select
                      id="rateControlSelect"
                      value={rateControlMode}
                      onChange={handleRateControlChange}
                      disabled={!serverSettings || serverSettings.rate_control_mode?.allowed?.length <= 1}
                    >
                      {(serverSettings?.rate_control_mode?.allowed || rateControlOptions).map((rc) => (
                        <option key={rc} value={rc}>
                          {displayLabel(rc)}
                        </option>
                      ))}
                    </select>
                  </div>
                )}
                {(isWebrtc || showFPS) && (renderableSettings.framerate ?? true) && (
                  <div className="dev-setting-item">
                    <label htmlFor="framerateSlider">
                      {t("sections.video.framerateLabel", {
                        framerate: framerate,
                      })}
                    </label>
                    <input
                      type="range"
                      id="framerateSlider"
                      min={serverSettings?.framerate?.min || 8}
                      max={serverSettings?.framerate?.max || 240}
                      step="1"
                      value={framerate}
                      onChange={handleFramerateChange}
                      disabled={!serverSettings || serverSettings.framerate?.min === serverSettings.framerate?.max}
                    />
                  </div>
                )}
                {showH264Options && appliedRateControlMode === RATE_CONTROL_CBR && (renderableSettings.video_bitrate ?? true) && (
                  <div className="dev-setting-item">
                    <label htmlFor="videoBitrateSlider">
                      {t("sections.video.bitrateLabel", {
                        bitrate: formatBitrate(videoBitrate),
                      })}
                    </label>
                    <input
                      type="range"
                      id="videoBitrateSlider"
                      min={0}
                      max={videoBitrateOptions.length - 1}
                      step="1"
                      value={bitrateSliderIndex}
                      onChange={handleVideoBitrateChange}
                      disabled={!serverSettings || serverSettings.video_bitrate?.min === serverSettings.video_bitrate?.max}
                    />
                  </div>
                )}
                {!isWebrtc && showJpegOptions && (
                  <>
                    {(renderableSettings.jpeg_quality ?? true) && (
                      <div className="dev-setting-item">
                        <label htmlFor="jpegQualitySlider">
                          {t("sections.video.jpegQualityLabel", {
                            jpegQuality: jpeg_quality,
                          })}
                        </label>
                        <input
                          type="range"
                          id="jpegQualitySlider"
                          min={serverSettings?.jpeg_quality?.min || 1}
                          max={serverSettings?.jpeg_quality?.max || 100}
                          step="1"
                          value={jpeg_quality}
                          onChange={handleJpegQualityChange}
                          disabled={!serverSettings || serverSettings.jpeg_quality?.min === serverSettings.jpeg_quality?.max}
                        />
                      </div>
                    )}
                  </>
                )}
                {showCRF && appliedRateControlMode === RATE_CONTROL_CRF && (renderableSettings.video_crf ?? true) && (
                  <div className="dev-setting-item">
                    <label htmlFor="videoCRFSlider">
                      {t("sections.video.crfLabel", { crf: video_crf })}
                    </label>
                    <input
                      type="range"
                      id="videoCRFSlider"
                      min={serverSettings?.video_crf?.min || 5}
                      max={serverSettings?.video_crf?.max || 50}
                      step="1"
                      value={video_crf}
                      onChange={handleVideoCRFChange}
                      disabled={!serverSettings || serverSettings.video_crf?.min === serverSettings.video_crf?.max}
                      style={{ direction: 'rtl' }}
                    />
                  </div>
                )}
                {showPaintOverQualityToggle && (renderableSettings.usePaintOverQuality ?? true) && (
                  <div className="dev-setting-item toggle-item">
                    <label htmlFor="usePaintOverQualityToggle">
                      {t("sections.video.usePaintOverQualityLabel", "Use Paint-Over Quality")}
                    </label>
                    <button
                      id="usePaintOverQualityToggle"
                      className={`toggle-button-sidebar ${usePaintOverQuality ? "active" : ""}`}
                      onClick={handleUsePaintOverQualityToggle}
                      aria-pressed={usePaintOverQuality}
                      disabled={!serverSettings || serverSettings.use_paint_over_quality?.locked}
                      title={t(usePaintOverQuality ? "buttons.usePaintOverQualityDisableTitle" : "buttons.usePaintOverQualityEnableTitle")}
                    >
                      <span className="toggle-button-sidebar-knob"></span>
                    </button>
                  </div>
                )}
                {showCRF && usePaintOverQuality && (renderableSettings.videoPaintoverCRF ?? true) && (
                  <div className="dev-setting-item">
                    <label htmlFor="videoPaintoverCRFSlider">
                      {t("sections.video.paintoverCrfLabel", { crf: videoPaintoverCRF })}
                    </label>
                    <input
                      type="range"
                      id="videoPaintoverCRFSlider"
                      min={serverSettings?.video_paintover_crf?.min || 5}
                      max={serverSettings?.video_paintover_crf?.max || 50}
                      step="1"
                      value={videoPaintoverCRF}
                      onChange={handleH264PaintoverCRFChange}
                      disabled={!serverSettings || serverSettings.video_paintover_crf?.min === serverSettings.video_paintover_crf?.max}
                      style={{ direction: 'rtl' }}
                    />
                  </div>
                )}
                {showH264Options && usePaintOverQuality && (renderableSettings.videoPaintoverBurstFrames ?? true) && (
                  <div className="dev-setting-item">
                    <label htmlFor="videoPaintoverBurstSlider">
                      {t("sections.video.paintoverBurstLabel", { frames: videoPaintoverBurstFrames }, `Paint-over Burst Frames: ${videoPaintoverBurstFrames}`)}
                    </label>
                    <input
                      type="range"
                      id="videoPaintoverBurstSlider"
                      min={serverSettings?.video_paintover_burst_frames?.min || 1}
                      max={serverSettings?.video_paintover_burst_frames?.max || 30}
                      step="1"
                      value={videoPaintoverBurstFrames}
                      onChange={handleH264PaintoverBurstChange}
                      disabled={!serverSettings || serverSettings.video_paintover_burst_frames?.min === serverSettings.video_paintover_burst_frames?.max}
                    />
                  </div>
                )}
                {!isWebrtc && showJpegOptions && usePaintOverQuality && (renderableSettings.paint_over_jpeg_quality ?? true) && (
                  <div className="dev-setting-item">
                    <label htmlFor="paintOverJpegQualitySlider">
                      {t("sections.video.paintOverJpegQualityLabel", {
                        paintOverJpegQuality: paint_over_jpeg_quality,
                      })}
                    </label>
                    <input
                      type="range"
                      id="paintOverJpegQualitySlider"
                      min={serverSettings?.paint_over_jpeg_quality?.min || 1}
                      max={serverSettings?.paint_over_jpeg_quality?.max || 100}
                      step="1"
                      value={paint_over_jpeg_quality}
                      onChange={handlePaintOverJpegQualityChange}
                      disabled={!serverSettings || serverSettings.paint_over_jpeg_quality?.min === serverSettings.paint_over_jpeg_quality?.max}
                    />
                  </div>
                )}
                {showH264Options && (renderableSettings.videoStreamingMode ?? true) && (
                  <div className="dev-setting-item toggle-item">
                    <label 
                      htmlFor="videoStreamingModeToggle"
                      title={t("sections.video.streamingModeDetails")}
                    >
                      {t("sections.video.streamingModeLabel", "Turbo")}
                    </label>
                    <button
                      id="videoStreamingModeToggle"
                      className={`toggle-button-sidebar ${videoStreamingMode ? "active" : ""}`}
                      onClick={handleH264StreamingModeToggle}
                      aria-pressed={videoStreamingMode}
                      disabled={!serverSettings || serverSettings.video_streaming_mode?.locked}
                      title={t(videoStreamingMode ? "buttons.videoStreamingModeDisableTitle" : "buttons.videoStreamingModeEnableTitle")}
                    >
                      <span className="toggle-button-sidebar-knob"></span>
                    </button>
                  </div>
                )}
                {showH264Options && (renderableSettings.videoFullColor ?? true) && (
                  <div className="dev-setting-item toggle-item">
                    <label htmlFor="videoFullColorToggle">
                      {t("sections.video.fullColorLabel")}
                    </label>
                    <button
                      id="videoFullColorToggle"
                      className={`toggle-button-sidebar ${videoFullColor ? "active" : ""}`}
                      onClick={handleH264FullColorToggle}
                      aria-pressed={videoFullColor}
                      disabled={!serverSettings || serverSettings.video_fullcolor?.locked}
                      title={t(videoFullColor ? "buttons.videoFullColorDisableTitle" : "buttons.videoFullColorEnableTitle")}
                    >
                      <span className="toggle-button-sidebar-knob"></span>
                    </button>
                  </div>
                )}
                {/* use_cpu only matters for full-frame h264enc; the server forces it true
                    for jpeg and striped on both transports. */}
                {activeEncoder === 'h264enc' && (renderableSettings.use_cpu ?? true) && (
                  <div className="dev-setting-item toggle-item">
                    <label htmlFor="useCpuToggle">
                      {t("sections.video.useCpuLabel", "CPU Encoding")}
                    </label>
                    <button
                      id="useCpuToggle"
                      className={`toggle-button-sidebar ${use_cpu ? "active" : ""}`}
                      onClick={handleUseCpuToggle}
                      aria-pressed={use_cpu}
                      disabled={!serverSettings || serverSettings.use_cpu?.locked}
                      title={t(use_cpu ? "buttons.useCpuDisableTitle" : "buttons.useCpuEnableTitle")}
                    >
                      <span className="toggle-button-sidebar-knob"></span>
                    </button>
                  </div>
                )}
              </div>
            )}
          </div>
        )}

        {!isViewerRole && (renderableSettings.screenSettings ?? true) && (
          <div className="sidebar-section">
            <div
              className="sidebar-section-header"
              onClick={() => toggleSection("screenSettings")}
              role="button"
              aria-expanded={sectionsOpen.screenSettings}
              aria-controls="screen-settings-content"
              tabIndex="0"
              onKeyDown={(e) =>
                (e.key === "Enter" || e.key === " ") &&
                toggleSection("screenSettings")
              }
            >
              <h3>{t("sections.screen.title")}</h3>
              <span className="section-toggle-icon">
                {sectionsOpen.screenSettings ? (
                  <CaretUpIcon />
                ) : (
                  <CaretDownIcon />
                )}
              </span>
            </div>
            {sectionsOpen.screenSettings && (
              <div
                className="sidebar-section-content"
                id="screen-settings-content"
              >
                {!isSecondaryDisplay && (
                  <>
                    {serverSettings?.second_screen?.value && (
                      <button
                        className="resolution-button toggle-button"
                        onClick={handleAddScreenClick}
                        style={{ marginBottom: "10px" }}
                        title={t("sections.screen.addScreenTitle", "Add a second screen")}
                      >
                        {t("sections.screen.addScreenButton", "Add Screen +")}
                      </button>
                    )}
                    {(renderableSettings.hidpi ?? true) && (
                      <div className="dev-setting-item toggle-item">
                        <label htmlFor="hidpiToggle">
                          {t("sections.screen.hidpiLabel", "HiDPI (Pixel Perfect)")}
                        </label>
                        <button
                          id="hidpiToggle"
                          className={`toggle-button-sidebar ${hidpiEnabled ? "active" : ""}`}
                          onClick={handleHidpiToggle}
                          aria-pressed={hidpiEnabled}
                          disabled={serverSettings?.enable_resize?.value === false}
                          title={serverSettings?.enable_resize?.value === false
                            ? t("sections.screen.hidpiDisabledNoResizeTitle", "Resolution changes are disabled by the server (enable_resize)")
                            : t(hidpiEnabled ? "sections.screen.hidpiDisableTitle" : "sections.screen.hidpiEnableTitle",
                                hidpiEnabled ? "Disable HiDPI (Use CSS Scaling)" : "Enable HiDPI (Pixel Perfect)")}
                        >
                          <span className="toggle-button-sidebar-knob"></span>
                        </button>
                      </div>
                    )}
                    {(renderableSettings.forceAlignedResolution ?? true) && (
                      <div className="dev-setting-item toggle-item">
                        <label
                          htmlFor="forceAlignedResolutionToggle"
                          title={t("sections.screen.forceAlignedResolutionDetails", "Forces the display resolution to be a multiple of 16 pixels")}
                        >
                          {t("sections.screen.forceAlignedResolutionLabel", "Force Aligned Resolution")}
                        </label>
                        <button
                          id="forceAlignedResolutionToggle"
                          className={`toggle-button-sidebar ${forceAlignedResolution ? "active" : ""}`}
                          onClick={handleForceAlignedResolutionToggle}
                          aria-pressed={forceAlignedResolution}
                          disabled={!serverSettings || serverSettings.force_aligned_resolution?.locked}
                          title={t(forceAlignedResolution ? "sections.screen.forceAlignedResolutionDisableTitle" : "sections.screen.forceAlignedResolutionEnableTitle", forceAlignedResolution ? "Disable Force Aligned Resolution" : "Enable Force Aligned Resolution")}
                        >
                          <span className="toggle-button-sidebar-knob"></span>
                        </button>
                      </div>
                    )}
                    <div className="dev-setting-item toggle-item">
                      <label htmlFor="antiAliasingToggle">
                        {t("sections.screen.antiAliasingLabel", "Anti-aliasing")}
                      </label>
                      <button
                        id="antiAliasingToggle"
                        className={`toggle-button-sidebar ${antiAliasing ? "active" : ""}`}
                        onClick={handleAntiAliasingToggle}
                        aria-pressed={antiAliasing}
                        title={t(antiAliasing ? "sections.screen.antiAliasingDisableTitle" : "sections.screen.antiAliasingEnableTitle",
                                  antiAliasing ? "Disable anti-aliasing (force pixelated)" : "Enable anti-aliasing (smooth on scaling)")}
                      >
                        <span className="toggle-button-sidebar-knob"></span>
                      </button>
                    </div>
                    {(renderableSettings.use_browser_cursors ?? true) && (
                      <div className="dev-setting-item toggle-item">
                        <label htmlFor="useBrowserCursorsToggle">
                          {t("sections.screen.useNativeCursorStylesLabel", "Use CSS cursors")}
                        </label>
                        <button
                          id="useBrowserCursorsToggle"
                          className={`toggle-button-sidebar ${(effectiveCursor !== null ? effectiveCursor : use_browser_cursors) ? "active" : ""}`}
                          onClick={handleUseBrowserCursorsToggle}
                          aria-pressed={effectiveCursor !== null ? effectiveCursor : use_browser_cursors}
                          title={t(use_browser_cursors ? "sections.screen.useNativeCursorStylesDisableTitle" : "sections.screen.useNativeCursorStylesEnableTitle",
                                  use_browser_cursors ? "Use canvas cursor rendering (Paint to canvas)" : "Use CSS cursor rendering (Replace system cursors)")}
                        >
                          <span className="toggle-button-sidebar-knob"></span>
                        </button>
                      </div>
                    )}
                    {(renderableSettings.uiScaling ?? true) && (
                      <div className="dev-setting-item">
                        <label htmlFor="uiScalingSelect">
                          {t("sections.screen.uiScalingLabel", "UI Scaling")}
                        </label>
                        <select
                          id="uiScalingSelect"
                          value={selectedDpi}
                          onChange={handleDpiScalingChange}
                          disabled={!serverSettings || serverSettings.scaling_dpi?.allowed?.length <= 1
                            || serverSettings.scaling_dpi?.overridden === true}
                        >
                          {(serverSettings?.scaling_dpi?.allowed || []).map((dpiValue) => {
                            const percent = Math.round((parseInt(dpiValue, 10) / 96) * 100);
                            const label = `${percent}%`;
                            return (
                              <option key={dpiValue} value={dpiValue}>
                                {dpiValue === String(currentDeviceDpi) ? `${label} *` : label}
                              </option>
                            );
                          })}
                        </select>
                      </div>
                    )}
                  </>
                )}
                {(!serverSettings?.manual_resolution?.locked) && (
                  <>
                    <div className="dev-setting-item">
                      <label htmlFor="resolutionPresetSelect">
                        {t("sections.screen.presetLabel")}
                      </label>
                      <select
                        id="resolutionPresetSelect"
                        value={presetValue}
                        onChange={handlePresetChange}
                      >
                        {translatedCommonResolutions.map((res, i) => (
                          <option key={i} value={res.value} disabled={i === 0}>
                            {res.text}
                          </option>
                        ))}
                      </select>
                    </div>
                    <div className="resolution-manual-inputs">
                      <div className="dev-setting-item manual-input-item">
                        <label htmlFor="manualWidthInput">
                          {t("sections.screen.widthLabel")}
                        </label>
                        <input
                          className="allow-native-input"
                          type="number"
                          id="manualWidthInput"
                          min="1"
                          step="2"
                          placeholder={t("sections.screen.widthPlaceholder")}
                          value={manual_width}
                          onChange={handleManualWidthChange}
                        />
                      </div>
                      <div className="dev-setting-item manual-input-item">
                        <label htmlFor="manualHeightInput">
                          {t("sections.screen.heightLabel")}
                        </label>
                        <input
                          className="allow-native-input"
                          type="number"
                          id="manualHeightInput"
                          min="1"
                          step="2"
                          placeholder={t("sections.screen.heightPlaceholder")}
                          value={manual_height}
                          onChange={handleManualHeightChange}
                        />
                      </div>
                    </div>
                    <div className="resolution-action-buttons">
                      <button
                        className="resolution-button"
                        onClick={handleSetManualResolution}
                      >
                        {t("sections.screen.setManualButton")}
                      </button>
                      <button
                        className="resolution-button reset-button"
                        onClick={handleResetResolution}
                      >
                        {t("sections.screen.resetButton")}
                      </button>
                    </div>
                  </>
                )}
                <button
                  className={`resolution-button toggle-button ${
                    scaleLocally ? "active" : ""
                  }`}
                  onClick={handleScaleLocallyToggle}
                  style={{ marginTop: "10px" }}
                  title={t(
                    scaleLocally
                      ? "sections.screen.scaleLocallyTitleDisable"
                      : "sections.screen.scaleLocallyTitleEnable"
                  )}
                >
                  {t("sections.screen.scaleLocallyLabel")}{" "}
                  {t(
                    scaleLocally
                      ? "sections.screen.scaleLocallyOn"
                      : "sections.screen.scaleLocallyOff"
                  )}
                </button>
              </div>
            )}
          </div>
        )}

        {!isSecondaryDisplay && (
          <>
            {(renderableSettings.audioSettings ?? true) && (
              <div className="sidebar-section">
                <div
                  className="sidebar-section-header"
                  onClick={() => toggleSection("audioSettings")}
                  role="button"
                  aria-expanded={sectionsOpen.audioSettings}
                  aria-controls="audio-settings-content"
                  tabIndex="0"
                  onKeyDown={(e) => (e.key === "Enter" || e.key === " ") &&
                    toggleSection("audioSettings")}
                >
                  <h3>{t("sections.audio.title")}</h3>
                  <span className="section-toggle-icon">
                    {isLoadingAudioDevices ? (
                      <SpinnerIcon />
                    ) : sectionsOpen.audioSettings ? (
                      <CaretUpIcon />
                    ) : (
                      <CaretDownIcon />
                    )}
                  </span>
                </div>
                {sectionsOpen.audioSettings && (
                  <div
                    className="sidebar-section-content"
                    id="audio-settings-content"
                  >
                    {audioDeviceError && (
                      <div className="error-message">{audioDeviceError}</div>
                    )}
                    <div className="dev-setting-item">
                      <label htmlFor="audioInputSelect">
                        {t("sections.audio.inputLabel")}
                      </label>
                      <select
                        id="audioInputSelect"
                        value={selectedInputDeviceId}
                        onChange={handleAudioInputChange}
                        disabled={isLoadingAudioDevices || !!audioDeviceError}
                        className="audio-device-select"
                      >
                        {audioInputDevices.map((d) => (
                          <option key={d.deviceId} value={d.deviceId}>
                            {d.label}
                          </option>
                        ))}
                      </select>
                    </div>
                    {isOutputSelectionSupported && (
                      <div className="dev-setting-item">
                        <label htmlFor="audioOutputSelect">
                          {t("sections.audio.outputLabel")}
                        </label>
                        <select
                          id="audioOutputSelect"
                          value={selectedOutputDeviceId}
                          onChange={handleAudioOutputChange}
                          disabled={isLoadingAudioDevices || !!audioDeviceError}
                          className="audio-device-select"
                        >
                          {audioOutputDevices.map((d) => (
                            <option key={d.deviceId} value={d.deviceId}>
                              {d.label}
                            </option>
                          ))}
                        </select>
                      </div>
                    )}
                    {(renderableSettings.audio_bitrate ?? true) && (
                      <div className="dev-setting-item">
                        <label htmlFor="audioBitrateSlider">
                          {t("sections.audio.bitrateLabel", {
                            bitrate: audioBitrate/ 1000,
                          })}
                        </label>
                        <input
                          type="range"
                          id="audioBitrateSlider"
                          min={0}
                          max={audioBitrateChoices.length - 1}
                          step={1}
                          value={Math.max(0, audioBitrateChoices.indexOf(audioBitrate))}
                          onChange={(e) => handleAudioBitrateChange(audioBitrateChoices[parseInt(e.target.value, 10)])}
                          disabled={!serverSettings || (serverSettings.audio_bitrate?.allowed?.length ?? 0) <= 1}
                        />
                      </div>
                    )}
                    {!isOutputSelectionSupported &&
                      !isLoadingAudioDevices &&
                      !audioDeviceError && (
                        <p className="device-support-notice">
                          {t("sections.audio.outputNotSupported")}
                        </p>
                      )}
                  </div>
                )}
              </div>
            )}
            {(renderableSettings.stats ?? true) && (
              <div className="sidebar-section">
                <div
                  className="sidebar-section-header"
                  onClick={() => toggleSection("stats")}
                  role="button"
                  aria-expanded={sectionsOpen.stats}
                  aria-controls="stats-content"
                  tabIndex="0"
                  onKeyDown={(e) => (e.key === "Enter" || e.key === " ") && toggleSection("stats")}
                >
                  <h3>{t("sections.stats.title")}</h3>
                  <span className="section-toggle-icon">
                    {sectionsOpen.stats ? <CaretUpIcon /> : <CaretDownIcon />}
                  </span>
                </div>
                {sectionsOpen.stats && (
                  <div className="sidebar-section-content" id="stats-content">
                    <div className="stats-gauges">
                      <div
                        className="gauge-container"
                        onMouseEnter={(e) => handleMouseEnter(e, "cpu")}
                        onMouseLeave={handleMouseLeave}
                      >
                        <svg
                          width={gaugeSize}
                          height={gaugeSize}
                          viewBox={`0 0 ${gaugeSize} ${gaugeSize}`}
                        >
                          <circle
                            stroke="var(--item-border)"
                            fill="transparent"
                            strokeWidth={gaugeStrokeWidth}
                            r={gaugeRadius}
                            cx={gaugeCenter}
                            cy={gaugeCenter} />
                          <circle
                            stroke="var(--sidebar-header-color)"
                            fill="transparent"
                            strokeWidth={gaugeStrokeWidth}
                            r={gaugeRadius}
                            cx={gaugeCenter}
                            cy={gaugeCenter}
                            transform={`rotate(-90 ${gaugeCenter} ${gaugeCenter})`}
                            style={{
                              strokeDasharray: gaugeCircumference,
                              strokeDashoffset: cpuOffset,
                              transition: "stroke-dashoffset 0.3s ease-in-out",
                              strokeLinecap: "round",
                            }} />
                          <text
                            x={gaugeCenter}
                            y={gaugeCenter}
                            textAnchor="middle"
                            dominantBaseline="central"
                            fontSize={`${gaugeSize / 5}px`}
                            fill="var(--sidebar-text)"
                            fontWeight="bold"
                          >
                            {Math.round(
                              Math.max(0, Math.min(100, cpuPercent || 0))
                            )}%
                          </text>
                        </svg>
                        <div className="gauge-label">
                          {t("sections.stats.cpuLabel")}
                        </div>
                      </div>
                      <div
                        className="gauge-container"
                        onMouseEnter={(e) => handleMouseEnter(e, "sysmem")}
                        onMouseLeave={handleMouseLeave}
                      >
                        <svg
                          width={gaugeSize}
                          height={gaugeSize}
                          viewBox={`0 0 ${gaugeSize} ${gaugeSize}`}
                        >
                          <circle
                            stroke="var(--item-border)"
                            fill="transparent"
                            strokeWidth={gaugeStrokeWidth}
                            r={gaugeRadius}
                            cx={gaugeCenter}
                            cy={gaugeCenter} />
                          <circle
                            stroke="var(--sidebar-header-color)"
                            fill="transparent"
                            strokeWidth={gaugeStrokeWidth}
                            r={gaugeRadius}
                            cx={gaugeCenter}
                            cy={gaugeCenter}
                            transform={`rotate(-90 ${gaugeCenter} ${gaugeCenter})`}
                            style={{
                              strokeDasharray: gaugeCircumference,
                              strokeDashoffset: sysMemOffset,
                              transition: "stroke-dashoffset 0.3s ease-in-out",
                              strokeLinecap: "round",
                            }} />
                          <text
                            x={gaugeCenter}
                            y={gaugeCenter}
                            textAnchor="middle"
                            dominantBaseline="central"
                            fontSize={`${gaugeSize / 5}px`}
                            fill="var(--sidebar-text)"
                            fontWeight="bold"
                          >
                            {Math.round(
                              Math.max(0, Math.min(100, sysMemPercent || 0))
                            )}
                            %
                          </text>
                        </svg>
                        <div className="gauge-label">
                          {t("sections.stats.sysMemLabel")}
                        </div>
                      </div>
                      {window.gpu_stats && (
                        <>
                          <div
                            className="gauge-container"
                            onMouseEnter={(e) => handleMouseEnter(e, "gpu")}
                            onMouseLeave={handleMouseLeave}
                          >
                            <svg
                              width={gaugeSize}
                              height={gaugeSize}
                              viewBox={`0 0 ${gaugeSize} ${gaugeSize}`}
                            >
                              <circle
                                stroke="var(--item-border)"
                                fill="transparent"
                                strokeWidth={gaugeStrokeWidth}
                                r={gaugeRadius}
                                cx={gaugeCenter}
                                cy={gaugeCenter} />
                              <circle
                                stroke="var(--sidebar-header-color)"
                                fill="transparent"
                                strokeWidth={gaugeStrokeWidth}
                                r={gaugeRadius}
                                cx={gaugeCenter}
                                cy={gaugeCenter}
                                transform={`rotate(-90 ${gaugeCenter} ${gaugeCenter})`}
                                style={{
                                  strokeDasharray: gaugeCircumference,
                                  strokeDashoffset: gpuOffset,
                                  transition: "stroke-dashoffset 0.3s ease-in-out",
                                  strokeLinecap: "round",
                                }} />
                              <text
                                x={gaugeCenter}
                                y={gaugeCenter}
                                textAnchor="middle"
                                dominantBaseline="central"
                                fontSize={`${gaugeSize / 5}px`}
                                fill="var(--sidebar-text)"
                                fontWeight="bold"
                              >
                                {Math.round(
                                  Math.max(0, Math.min(100, gpuPercent || 0))
                                )}%
                              </text>
                            </svg>
                            <div className="gauge-label">
                              {t("sections.stats.gpuLabel")}
                            </div>
                          </div>
                          <div
                            className="gauge-container"
                            onMouseEnter={(e) => handleMouseEnter(e, "gpumem")}
                            onMouseLeave={handleMouseLeave}
                          >
                            <svg
                              width={gaugeSize}
                              height={gaugeSize}
                              viewBox={`0 0 ${gaugeSize} ${gaugeSize}`}
                            >
                              <circle
                                stroke="var(--item-border)"
                                fill="transparent"
                                strokeWidth={gaugeStrokeWidth}
                                r={gaugeRadius}
                                cx={gaugeCenter}
                                cy={gaugeCenter} />
                              <circle
                                stroke="var(--sidebar-header-color)"
                                fill="transparent"
                                strokeWidth={gaugeStrokeWidth}
                                r={gaugeRadius}
                                cx={gaugeCenter}
                                cy={gaugeCenter}
                                transform={`rotate(-90 ${gaugeCenter} ${gaugeCenter})`}
                                style={{
                                  strokeDasharray: gaugeCircumference,
                                  strokeDashoffset: gpuMemOffset,
                                  transition: "stroke-dashoffset 0.3s ease-in-out",
                                  strokeLinecap: "round",
                                }} />
                              <text
                                x={gaugeCenter}
                                y={gaugeCenter}
                                textAnchor="middle"
                                dominantBaseline="central"
                                fontSize={`${gaugeSize / 5}px`}
                                fill="var(--sidebar-text)"
                                fontWeight="bold"
                              >
                                {Math.round(
                                  Math.max(0, Math.min(100, gpuMemPercent || 0))
                                )}
                                %
                              </text>
                            </svg>
                            <div className="gauge-label">
                              {t("sections.stats.gpuMemLabel")}
                            </div>
                          </div>
                        </>
                      )}
                      <div
                        className="gauge-container"
                        onMouseEnter={(e) => handleMouseEnter(e, "fps")}
                        onMouseLeave={handleMouseLeave}
                      >
                        <svg
                          width={gaugeSize}
                          height={gaugeSize}
                          viewBox={`0 0 ${gaugeSize} ${gaugeSize}`}
                        >
                          <circle
                            stroke="var(--item-border)"
                            fill="transparent"
                            strokeWidth={gaugeStrokeWidth}
                            r={gaugeRadius}
                            cx={gaugeCenter}
                            cy={gaugeCenter} />
                          <circle
                            stroke="var(--sidebar-header-color)"
                            fill="transparent"
                            strokeWidth={gaugeStrokeWidth}
                            r={gaugeRadius}
                            cx={gaugeCenter}
                            cy={gaugeCenter}
                            transform={`rotate(-90 ${gaugeCenter} ${gaugeCenter})`}
                            style={{
                              strokeDasharray: gaugeCircumference,
                              strokeDashoffset: fpsOffset,
                              transition: "stroke-dashoffset 0.3s ease-in-out",
                              strokeLinecap: "round",
                            }} />
                          <text
                            x={gaugeCenter}
                            y={gaugeCenter}
                            textAnchor="middle"
                            dominantBaseline="central"
                            fontSize={`${gaugeSize / 5}px`}
                            fill="var(--sidebar-text)"
                            fontWeight="bold"
                          >
                            {clientFps}
                          </text>
                        </svg>
                        <div className="gauge-label">
                          {t("sections.stats.fpsLabel")}
                        </div>
                      </div>
                      {(<div
                        className="gauge-container"
                        onMouseEnter={(e) => handleMouseEnter(e, "audio")}
                        onMouseLeave={handleMouseLeave}
                      >
                        <svg
                          width={gaugeSize}
                          height={gaugeSize}
                          viewBox={`0 0 ${gaugeSize} ${gaugeSize}`}
                        >
                          <circle
                            stroke="var(--item-border)"
                            fill="transparent"
                            strokeWidth={gaugeStrokeWidth}
                            r={gaugeRadius}
                            cx={gaugeCenter}
                            cy={gaugeCenter} />
                          <circle
                            stroke="var(--sidebar-header-color)"
                            fill="transparent"
                            strokeWidth={gaugeStrokeWidth}
                            r={gaugeRadius}
                            cx={gaugeCenter}
                            cy={gaugeCenter}
                            transform={`rotate(-90 ${gaugeCenter} ${gaugeCenter})`}
                            style={{
                              strokeDasharray: gaugeCircumference,
                              strokeDashoffset: audioLevelOffset,
                              transition: "stroke-dashoffset 0.3s ease-in-out",
                              strokeLinecap: "round",
                            }} />
                          <text
                            x={gaugeCenter}
                            y={gaugeCenter}
                            textAnchor="middle"
                            dominantBaseline="central"
                            fontSize={`${gaugeSize / 5}px`}
                            fill="var(--sidebar-text)"
                            fontWeight="bold"
                          >
                            {audioLevel}
                          </text>
                        </svg>
                        <div className="gauge-label">
                          {t("sections.stats.audioLabel")}
                        </div>
                      </div>)}
                      <div
                        className="gauge-container"
                        onMouseEnter={(e) => handleMouseEnter(e, "bandwidth")}
                        onMouseLeave={handleMouseLeave}
                      >
                        <svg
                          width={gaugeSize}
                          height={gaugeSize}
                          viewBox={`0 0 ${gaugeSize} ${gaugeSize}`}
                        >
                          <circle
                            stroke="var(--item-border)"
                            fill="transparent"
                            strokeWidth={gaugeStrokeWidth}
                            r={gaugeRadius}
                            cx={gaugeCenter}
                            cy={gaugeCenter} />
                          <circle
                            stroke="var(--sidebar-header-color)"
                            fill="transparent"
                            strokeWidth={gaugeStrokeWidth}
                            r={gaugeRadius}
                            cx={gaugeCenter}
                            cy={gaugeCenter}
                            transform={`rotate(-90 ${gaugeCenter} ${gaugeCenter})`}
                            style={{
                              strokeDasharray: gaugeCircumference,
                              strokeDashoffset: bandwidthOffset,
                              transition: "stroke-dashoffset 0.3s ease-in-out",
                              strokeLinecap: "round",
                            }} />
                          <text
                            x={gaugeCenter}
                            y={gaugeCenter}
                            textAnchor="middle"
                            dominantBaseline="central"
                            fontSize={`${gaugeSize / 5}px`}
                            fill="var(--sidebar-text)"
                            fontWeight="bold"
                          >
                            {Math.round(bandwidthMbps)}
                          </text>
                        </svg>
                        <div className="gauge-label">
                          {t("sections.stats.bandwidthLabel", "Bandwidth")}
                        </div>
                      </div>
                      <div
                        className="gauge-container"
                        onMouseEnter={(e) => handleMouseEnter(e, "latency")}
                        onMouseLeave={handleMouseLeave}
                      >
                        <svg
                          width={gaugeSize}
                          height={gaugeSize}
                          viewBox={`0 0 ${gaugeSize} ${gaugeSize}`}
                        >
                          <circle
                            stroke="var(--item-border)"
                            fill="transparent"
                            strokeWidth={gaugeStrokeWidth}
                            r={gaugeRadius}
                            cx={gaugeCenter}
                            cy={gaugeCenter} />
                          <circle
                            stroke="var(--sidebar-header-color)"
                            fill="transparent"
                            strokeWidth={gaugeStrokeWidth}
                            r={gaugeRadius}
                            cx={gaugeCenter}
                            cy={gaugeCenter}
                            transform={`rotate(-90 ${gaugeCenter} ${gaugeCenter})`}
                            style={{
                              strokeDasharray: gaugeCircumference,
                              strokeDashoffset: latencyOffset,
                              transition: "stroke-dashoffset 0.3s ease-in-out",
                              strokeLinecap: "round",
                            }} />
                          <text
                            x={gaugeCenter}
                            y={gaugeCenter}
                            textAnchor="middle"
                            dominantBaseline="central"
                            fontSize={`${gaugeSize / 5}px`}
                            fill="var(--sidebar-text)"
                            fontWeight="bold"
                          >
                            {Math.round(latencyMs)}
                          </text>
                        </svg>
                        <div className="gauge-label">
                          {t("sections.stats.latencyLabel", "Latency")}
                        </div>
                      </div>
                    </div>
                  </div>
                )}
              </div>
            )}

            {(renderableSettings.clipboard ?? true) && (
              <div className="sidebar-section">
                <div
                  className="sidebar-section-header"
                  onClick={() => toggleSection("clipboard")}
                  role="button"
                  aria-expanded={sectionsOpen.clipboard}
                  aria-controls="clipboard-content"
                  tabIndex="0"
                  onKeyDown={(e) =>
                    (e.key === "Enter" || e.key === " ") && toggleSection("clipboard")
                  }
                >
                  <h3>{t("sections.clipboard.title")}</h3>
                  <span className="section-toggle-icon">
                    {sectionsOpen.clipboard ? <CaretUpIcon /> : <CaretDownIcon />}
                  </span>
                </div>
                {sectionsOpen.clipboard && (
                  <div className="sidebar-section-content" id="clipboard-content">
                    {(renderableSettings.binaryClipboard ?? true) && (
                      <div className="dev-setting-item toggle-item">
                        <label 
                          htmlFor="enableBinaryClipboardToggle"
                          title={t("sections.clipboard.binaryModeDetails")}
                        >
                          {t("sections.clipboard.binaryModeLabel", "Image Support")}
                        </label>
                        <button
                          id="enableBinaryClipboardToggle"
                          className={`toggle-button-sidebar ${enableBinaryClipboard ? "active" : ""}`}
                          onClick={handleEnableBinaryClipboardToggle}
                          aria-pressed={enableBinaryClipboard}
                          disabled={!serverSettings || serverSettings.enable_binary_clipboard?.locked}
                          title={t(enableBinaryClipboard ? "buttons.binaryClipboardDisableTitle" : "buttons.binaryClipboardEnableTitle")}
                        >
                          <span className="toggle-button-sidebar-knob"></span>
                        </button>
                      </div>
                    )}
                    <div className="dashboard-clipboard-item">
                      <label htmlFor="dashboardClipboardTextarea">
                        {t("sections.clipboard.label")}
                      </label>
                      <textarea
                        className="allow-native-input"
                        id="dashboardClipboardTextarea"
                        value={dashboardClipboardContent}
                        onChange={handleClipboardChange}
                        onBlur={handleClipboardBlur}
                        readOnly={dashboardClipboardTruncated}
                        rows="5"
                        placeholder={t("sections.clipboard.placeholder")}
                      />
                    </div>
                    {(renderableSettings.binaryClipboard ?? true) &&
                      enableBinaryClipboard && (
                        <div className="dashboard-clipboard-item">
                          <button
                            className="app-action-button install"
                            onClick={() => clipboardImageInputRef.current?.click()}
                          >
                            {t("sections.clipboard.uploadImage", "Upload Image")}
                          </button>
                          <input
                            ref={clipboardImageInputRef}
                            type="file"
                            accept="image/*"
                            onChange={handleClipboardImageUpload}
                            style={{ display: "none" }}
                          />
                        </div>
                      )}
                  </div>
                )}
              </div>
            )}
          </>
        )}

        {!isSecondaryDisplay && (
          <>
            {(renderableSettings.files ?? true) && (
              <div className="sidebar-section">
                <div
                  className="sidebar-section-header"
                  onClick={() => toggleSection("files")}
                  role="button"
                  aria-expanded={sectionsOpen.files}
                  aria-controls="files-content"
                  tabIndex="0"
                  onKeyDown={(e) =>
                    (e.key === "Enter" || e.key === " ") && toggleSection("files")
                  }
                >
                  <h3>{t("sections.files.title")}</h3>
                  <span className="section-toggle-icon">
                    {sectionsOpen.files ? <CaretUpIcon /> : <CaretDownIcon />}
                  </span>
                </div>
                {sectionsOpen.files && (
                  <div className="sidebar-section-content" id="files-content">
                    {(renderableSettings.fileUpload ?? true) && (
                      <button
                        className="resolution-button"
                        onClick={handleUploadClick}
                        style={{ marginTop: "5px", marginBottom: "5px" }}
                        title={t("sections.files.uploadButtonTitle")}
                      >
                        {t("sections.files.uploadButton")}
                      </button>
                    )}
                    {(renderableSettings.fileDownload ?? true) && (
                      <button
                        className="resolution-button"
                        onClick={toggleFilesModal}
                        style={{ marginTop: "5px", marginBottom: "5px" }}
                        title={t(
                          "sections.files.downloadButtonTitle",
                          "Download Files"
                        )}
                      >
                        {t("sections.files.downloadButtonTitle", "Download Files")}
                      </button>
                    )}
                  </div>
                )}
              </div>
            )}

            {(renderableSettings.apps ?? true) && (
              <div className="sidebar-section">
                <div
                  className="sidebar-section-header"
                  onClick={() => toggleSection("apps")}
                  role="button"
                  aria-expanded={sectionsOpen.apps}
                  aria-controls="apps-content"
                  tabIndex="0"
                  onKeyDown={(e) =>
                    (e.key === "Enter" || e.key === " ") && toggleSection("apps")
                  }
                >
                  <h3>{t("sections.apps.title", "Apps")}</h3>
                  <span className="section-toggle-icon">
                    {sectionsOpen.apps ? <CaretUpIcon /> : <CaretDownIcon />}
                  </span>
                </div>
                {sectionsOpen.apps && (
                  <div className="sidebar-section-content" id="apps-content">
                    <button
                      className="resolution-button"
                      onClick={toggleAppsModal}
                      style={{ marginTop: "5px", marginBottom: "5px" }}
                      title={t("sections.apps.openButtonTitle", "Manage Apps")}
                    >
                      <AppsIcon />
                      <span style={{ marginLeft: "8px" }}>
                        {t("sections.apps.openButton", "Manage Apps")}
                      </span>
                    </button>
                  </div>
                )}
              </div>
            )}

            {(renderableSettings.sharing ?? true) && (renderableSettings.enableSharing ?? true) && (
              <div className="sidebar-section">
                <div
                  className="sidebar-section-header"
                  onClick={() => toggleSection("sharing")}
                  role="button"
                  aria-expanded={sectionsOpen.sharing}
                  aria-controls="sharing-content"
                  tabIndex="0"
                  onKeyDown={(e) =>
                    (e.key === "Enter" || e.key === " ") &&
                    toggleSection("sharing")
                  }
                >
                  <h3>{t("sections.sharing.title", "Sharing")}</h3>
                  <span className="section-toggle-icon">
                    {sectionsOpen.sharing ? <CaretUpIcon /> : <CaretDownIcon />}
                  </span>
                </div>
                {sectionsOpen.sharing && (
                  <div className="sidebar-section-content" id="sharing-content">
                    {filteredSharingLinks.map((link) => {
                      const fullUrl = `${baseUrl}${link.hash}`;
                      return (
                        <div
                          key={link.id}
                          className="sharing-link-item"
                          title={link.tooltip}
                        >
                          <span className="sharing-link-label">
                            {link.label}
                          </span>
                          <div className="sharing-link-actions">
                            <a
                              href={fullUrl}
                              target="_blank"
                              rel="noopener noreferrer"
                              className="sharing-link"
                              title={t("sections.sharing.openLinkTitle", { label: link.label })}
                            >
                              {fullUrl}
                            </a>
                            <button
                              type="button"
                              onClick={() => handleCopyLink(fullUrl, link.label)}
                              className="copy-button"
                              title={t("sections.sharing.copyLinkTitle", { label: link.label })}
                            >
                              <CopyIcon />
                            </button>
                          </div>
                        </div>
                      );
                    })}
                  </div>
                )}
              </div>
            )}

            {(renderableSettings.gamepads ?? true) && (
              <div className="sidebar-section">
                <div
                  className="sidebar-section-header"
                  onClick={() => toggleSection("gamepads")}
                  role="button"
                  aria-expanded={sectionsOpen.gamepads}
                  aria-controls="gamepads-content"
                  tabIndex="0"
                  onKeyDown={(e) =>
                    (e.key === "Enter" || e.key === " ") &&
                    toggleSection("gamepads")
                  }
                >
                  <h3>{t("sections.gamepads.title", "Gamepads")}</h3>
                  <span className="section-toggle-icon" aria-hidden="true">
                    {sectionsOpen.gamepads ? <CaretUpIcon /> : <CaretDownIcon />}
                  </span>
                </div>
                {sectionsOpen.gamepads && (
                  <div className="sidebar-section-content" id="gamepads-content">
                    <div
                      className="dev-setting-item"
                      style={{ marginBottom: "10px" }}
                    >
                      <button
                        className={`resolution-button toggle-button ${
                          isTouchGamepadActive ? "active" : ""
                        }`}
                        onClick={handleToggleTouchGamepad}
                        title={t(
                          isTouchGamepadActive
                            ? "sections.gamepads.touchDisableTitle"
                            : "sections.gamepads.touchEnableTitle",
                          isTouchGamepadActive
                            ? "Disable Touch Gamepad"
                            : "Enable Touch Gamepad"
                        )}
                      >
                        <GamepadIcon />
                        <span style={{ marginLeft: "8px" }}>
                          {t(
                            isTouchGamepadActive
                              ? "sections.gamepads.touchActiveLabel"
                              : "sections.gamepads.touchInactiveLabel",
                            isTouchGamepadActive
                              ? "Touch Gamepad: ON"
                              : "Touch Gamepad: OFF"
                          )}
                        </span>
                      </button>
                    </div>

                    {isMobile && isTouchGamepadActive ? (
                      <p>
                        {t(
                          "sections.gamepads.physicalHiddenForTouch",
                          "Physical gamepad display is hidden while touch gamepad is active."
                        )}
                      </p>
                    ) : (
                      <>
                        {Object.keys(gamepadStates).length > 0 ? (
                          Object.keys(gamepadStates)
                            .sort((a, b) => parseInt(a, 10) - parseInt(b, 10))
                            .map((gpIndexStr) => {
                              const gpIndex = parseInt(gpIndexStr, 10);
                              return (
                                <GamepadVisualizer
                                  key={gpIndex}
                                  gamepadIndex={gpIndex}
                                  gamepadState={gamepadStates[gpIndex]}
                                />
                              );
                            })
                        ) : (
                          <p className="no-gamepads-message">
                            {isMobile
                              ? t(
                                  "sections.gamepads.noActivityMobileOrEnableTouch",
                                  "No physical gamepads. Enable touch gamepad or connect a controller."
                                )
                              : t(
                                  "sections.gamepads.noActivity",
                                  "No physical gamepad activity detected."
                                )}
                          </p>
                        )}
                      </>
                    )}
                  </div>
                )}
              </div>
            )}

            {(renderableSettings.shortcuts ?? true) && (
              <div className="sidebar-section">
                <div
                  className="sidebar-section-header"
                  onClick={() => toggleSection("shortcuts")}
                  role="button"
                  aria-expanded={sectionsOpen.shortcuts}
                  aria-controls="shortcuts-content"
                  tabIndex="0"
                  onKeyDown={(e) =>
                    (e.key === "Enter" || e.key === " ") &&
                    toggleSection("shortcuts")
                  }
                >
                  <h3>{t("sections.shortcuts.title", "Shortcuts")}</h3>
                  <span className="section-toggle-icon">
                    {sectionsOpen.shortcuts ? <CaretUpIcon /> : <CaretDownIcon />}
                  </span>
                </div>
                {sectionsOpen.shortcuts && (
                  <div className="sidebar-section-content" id="shortcuts-content">
                    {[
                      { combo: "Ctrl + Shift + F", label: t("sections.shortcuts.fullscreen", "Toggle fullscreen") },
                      { combo: "Ctrl + Shift + X", label: t("sections.shortcuts.gamingMode", "Toggle gaming mode") },
                      { combo: "Ctrl + Shift + M", label: t("sections.shortcuts.openMenu", "Open or close the dashboard") },
                      { combo: "Ctrl + Shift + G", label: t("sections.shortcuts.toggleGamepad", "Toggle the virtual gamepad") },
                      { combo: "Ctrl + Shift + Left Click", label: t("sections.shortcuts.pointerLock", "Lock the pointer to the stream") },
                    ].map((sc) => (
                      <div
                        key={sc.combo}
                        className="shortcut-item"
                        style={{
                          display: "flex",
                          flexDirection: "column",
                          alignItems: "center",
                          gap: "2px",
                          padding: "6px 0",
                          textAlign: "center",
                        }}
                      >
                        <kbd
                          style={{
                            fontFamily: "monospace",
                            whiteSpace: "normal",
                            overflowWrap: "anywhere",
                            maxWidth: "100%",
                            textAlign: "center",
                            padding: "2px 6px",
                            borderRadius: "4px",
                            border: "1px solid var(--item-border)",
                          }}
                        >
                          {sc.combo}
                        </kbd>
                        <span>{sc.label}</span>
                      </div>
                    ))}
                    <div style={{ marginTop: "8px", textAlign: "center" }}>
                      <a
                        className="cite-link"
                        href="https://docs.selkies.io/citation/"
                        target="_blank"
                        rel="noopener noreferrer"
                      >
                        {t("sections.shortcuts.citeNotice", "Cite our paper academically")}
                        {" ↗"}
                      </a>
                    </div>
                  </div>
                )}
              </div>
            )}
          </>
        )}
      </div>


      {hoveredItem && (
        <div
          className="gauge-tooltip"
          style={{
            left: `${tooltipPosition.x}px`,
            top: `${tooltipPosition.y}px`,
          }}
        >
          {getTooltipContent(hoveredItem)}
        </div>
      )}

      <div className={`notification-container theme-${theme}`}>
        {notifications.map((n) => (
          <div
            key={n.id}
            className={`notification-item ${n.status} ${
              n.fadingOut ? "fade-out" : ""
            }`}
            role="alert"
            aria-live="polite"
          >
            <div className="notification-header">
              <span className="notification-filename" title={n.fileName}>
                {n.fileName}
              </span>
              <button
                className="notification-close-button"
                onClick={() => removeNotification(n.id)}
                aria-label={t("notifications.closeButtonAlt", {
                  fileName: n.fileName,
                })}
              >
                &times;
              </button>
            </div>
            <div className="notification-body">
              {n.status === "progress" && (
                <>
                  <span className="notification-status-text">
                    {t("notifications.uploading", { progress: n.progress })}
                  </span>
                  <div className="notification-progress-bar-outer">
                    <div
                      className="notification-progress-bar-inner"
                      style={{ width: `${n.progress}%` }}
                    />
                  </div>
                </>
              )}
              {n.status === "end" && (
                <>
                  <span className="notification-status-text">
                    {n.message ? n.message : t("notifications.uploadComplete")}
                  </span>
                  <div className="notification-progress-bar-outer">
                    <div
                      className="notification-progress-bar-inner"
                      style={{ width: `100%` }}
                    />
                  </div>
                </>
              )}
              {n.status === "error" && (
                <>
                  <span className="notification-status-text error-text">
                    {t("notifications.uploadFailed")}
                  </span>
                  <div className="notification-progress-bar-outer">
                    <div
                      className="notification-progress-bar-inner"
                      style={{ width: `100%` }}
                    />
                  </div>
                  {n.message && (
                    <p className="notification-error-message">{n.message}</p>
                  )}
                </>
              )}
              {n.status === "warn" && (
                <>
                  {" "}
                  <span className="notification-status-text warn-text">
                    {n.message ? n.message : t("notifications.warningPrefix")}
                  </span>{" "}
                </>
              )}
            </div>
          </div>
        ))}
      </div>

      {isFilesModalOpen && (
        <div className={`files-modal theme-${theme}`}>
          <div className="files-modal-header">
            <button
              className="files-modal-close"
              onClick={toggleFilesModal}
              aria-label={t("filesModal.closeAlt")}
            >
              &times;
            </button>
          </div>
          <iframe src={withSessionToken("./api/files/")} title={t("filesModal.iframeTitle")} />
        </div>
      )}
      {isAppsModalOpen && (
        <AppsModal isOpen={isAppsModalOpen} onClose={toggleAppsModal} t={t}
          commandsAvailable={serverSettings?.command_enabled?.value === true}
          commandsKnown={serverSettings != null} />
      )}

      {(isMobile || hasDetectedTouch) && isKeyboardButtonVisible && (renderableSettings.keyboardButton ?? true) && (
        <button
          className={`virtual-keyboard-button theme-${theme} allow-native-input`}
          onClick={onKeyboardButtonClick}
          onPointerDown={handlePointerDown}
          onPointerMove={handlePointerMove}
          onPointerUp={handlePointerUp}
          onPointerCancel={handlePointerUp}
          style={{
            position: 'fixed',
            right: `${keyboardButtonPosition.right}px`,
            bottom: `${keyboardButtonPosition.bottom}px`,
            touchAction: 'none',
          }}
          title={t("buttons.virtualKeyboardButtonTitle", "Pop Keyboard")}
          aria-label={t("buttons.virtualKeyboardButtonTitle", "Pop Keyboard")}
        >
          <KeyboardIcon />
        </button>
      )}
    </>
  );
}

export default Sidebar;
