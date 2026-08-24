/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Selkies-specific utilities shared by the dashboard components, kept apart
 * from `lib/utils.ts`, which shadcn manages.
 *
 * Storage keys: every localStorage key is prefixed with the storage namespace
 * the streaming cores derive from the page URL (`lib/util.js`), and the
 * per-display settings gain a `_display2` suffix on the secondary display,
 * selected by the `#display2` URL hash. The route prefix is the cores'
 * derivation too, so both dashboards and both cores agree on all of them.
 *
 * Core state cache: the core broadcasts `serverSettings` once per connection
 * and `clipboardContentUpdate`, `effectiveCursorState` and
 * `audioDeviceSelected` only when something changes, but the panel components
 * (Settings, Sharing, Files, Clipboard) mount lazily when their menu opens,
 * after those messages. The latest of each is cached at module scope so a
 * late mount can seed its state synchronously; the panels' own listeners
 * still pick up later messages (reconnects, new clipboard events).
 * @module
 */

import { getRoutePrefix, getStorageAppName } from "../../selkies-web-core/lib/util.js";

export { getRoutePrefix, getStorageAppName };

/**
 * Union of both streaming cores' `PER_DISPLAY_SETTINGS` lists so the
 * dashboard and whichever core is running agree on which keys get the
 * `_display2` suffix. The websockets core alone owns `jpeg_quality` and
 * `paint_over_jpeg_quality` (jpeg is websockets framing); a key the running
 * core ignores is inert, while a missing one would make the secondary display
 * write the primary's key.
 */
const PER_DISPLAY_SETTINGS = [
  'framerate', 'video_crf', 'video_fullcolor',
  'video_streaming_mode', 'jpeg_quality', 'paint_over_jpeg_quality', 'use_cpu',
  'video_paintover_crf', 'video_paintover_burst_frames', 'use_paint_over_quality',
  'is_manual_resolution_mode', 'manual_width', 'manual_height',
  'encoder', 'scaleLocallyManual', 'use_browser_cursors', 'rate_control_mode',
  'video_bitrate', 'force_aligned_resolution',
];

const urlHash = typeof window !== 'undefined' ? window.location.hash : '';
/** Which display this page is, from the `#display2` URL hash. */
export const displayId = urlHash.startsWith('#display2') ? 'display2' : 'primary';
export const isSecondaryDisplay = displayId === 'display2';

/**
 * Viewer-designated URL modes, known before the server answers with a role.
 * The control UI keys off this for its first render so a shared viewer never
 * sees controls it cannot use in the gap before `clientRoleUpdate` arrives.
 */
export const isViewerUrlMode =
  urlHash.toLowerCase().startsWith('#shared') || /^#player[234]$/.test(urlHash.toLowerCase());

/**
 * Whether this is a mobile browser; the form factor is fixed for the life of
 * the document, so it is resolved once and available to the first render.
 */
export const isMobileClient = typeof window !== 'undefined' && !!(
  (navigator as any).userAgentData?.mobile ||
  /Mobi|Android|iPhone|iPad|iPod|BlackBerry|IEMobile|Opera Mini/i.test(navigator.userAgent)
);

const storageAppName = getStorageAppName();

/** The localStorage key for a setting on this display. */
export function getPrefixedKey(key: string): string {
  const prefixedKey = `${storageAppName}_${key}`;
  if (displayId === 'display2' && PER_DISPLAY_SETTINGS.includes(key)) {
    return `${prefixedKey}_display2`;
  }
  return prefixedKey;
}

let lastServerSettings: any = null;
let lastClipboardContent: { text: string; truncated: boolean } | null = null;
let lastEffectiveCursorState: boolean | null = null;
const lastAudioDevices: { input: string | null; output: string | null } = { input: null, output: null };
if (typeof window !== 'undefined') {
  window.addEventListener('message', (event: MessageEvent) => {
    if (event.origin !== window.location.origin) return;
    const message = event.data;
    if (typeof message !== 'object' || message === null) return;
    if (message.type === 'serverSettings') {
      lastServerSettings = message.payload;
    } else if (message.type === 'clipboardContentUpdate' && typeof message.text === 'string') {
      lastClipboardContent = { text: message.text, truncated: message.truncated === true };
    } else if (message.type === 'effectiveCursorState' && typeof message.value === 'boolean') {
      lastEffectiveCursorState = message.value;
    } else if (message.type === 'audioDeviceSelected' && message.deviceId) {
      if (message.context === 'input') lastAudioDevices.input = message.deviceId;
      else if (message.context === 'output') lastAudioDevices.output = message.deviceId;
    }
  });
}

/** The last `serverSettings` payload, or null before the first connection. */
export function getLastServerSettings(): any {
  return lastServerSettings;
}

/** The last server clipboard preview; the core emits it only on clipboard events. */
export function getLastClipboardContent(): { text: string; truncated: boolean } | null {
  return lastClipboardContent;
}

/**
 * The cursor mode actually in effect (multi-monitor forces browser cursors
 * on), emitted at connect and display-config time.
 */
export function getLastEffectiveCursorState(): boolean | null {
  return lastEffectiveCursorState;
}

/**
 * The dashboard's own audio device picks, which the core keeps for the life
 * of the page; a remounted Settings panel shows them again instead of
 * pretending the defaults are in use.
 */
export function getLastAudioDevices(): { input: string | null; output: string | null } {
  return lastAudioDevices;
}

/**
 * Whether a control is worth rendering: the user can actually change it,
 * meaning not locked and with more than one permitted value.
 */
export function isSettingRenderable(setting: any): boolean {
  if (!setting) return true;
  if (setting.locked === true) return false;
  if (setting.allowed && setting.allowed.length <= 1) return false;
  if (setting.min !== undefined && setting.max !== undefined && setting.min === setting.max) return false;
  return true;
}

/**
 * Derives every visibility flag the panels read from a `serverSettings`
 * payload: the admin's `ui_*` toggles, per-control renderability from each
 * setting's own constraints, the sharing roles, the stream-control menu
 * entries and the file-transfer directions.
 */
export function computeRenderableSettings(serverSettings: any): Record<string, any> {
  if (!serverSettings) return {};
  const s = serverSettings;

  const newRenderable: Record<string, any> = {};

  newRenderable.videoSettings = s.ui_sidebar_show_video_settings?.value ?? true;
  newRenderable.audioSettings = s.ui_sidebar_show_audio_settings?.value ?? true;
  newRenderable.screenSettings = s.ui_sidebar_show_screen_settings?.value ?? true;
  newRenderable.stats = s.ui_sidebar_show_stats?.value ?? true;
  newRenderable.clipboard = (s.ui_sidebar_show_clipboard?.value ?? true)
    && (s.clipboard_enabled?.value ?? true);
  newRenderable.files = s.ui_sidebar_show_files?.value ?? true;
  newRenderable.apps = s.ui_sidebar_show_apps?.value ?? true;
  newRenderable.sharing = (s.ui_sidebar_show_sharing?.value ?? true)
    && (s.enable_sharing?.value ?? true);
  newRenderable.fullscreen = s.ui_sidebar_show_fullscreen?.value ?? true;
  newRenderable.gamingMode = s.ui_sidebar_show_gaming_mode?.value ?? true;
  newRenderable.trackpad = s.ui_sidebar_show_trackpad?.value ?? true;
  newRenderable.keyboardButton = s.ui_sidebar_show_keyboard_button?.value ?? true;
  newRenderable.softButtons = s.ui_sidebar_show_soft_buttons?.value ?? true;
  newRenderable.coreButtons = s.ui_show_core_buttons?.value ?? true;
  newRenderable.shortcuts = s.ui_sidebar_show_shortcuts?.value ?? true;
  // Hides the floating gamepad card only, never the gamepad input toggle.
  newRenderable.gamepads = s.ui_sidebar_show_gamepads?.value ?? true;

  newRenderable.encoder = isSettingRenderable(s.encoder);
  newRenderable.framerate = isSettingRenderable(s.framerate);
  newRenderable.jpegQuality = isSettingRenderable(s.jpeg_quality);
  newRenderable.paintOverJpegQuality = isSettingRenderable(s.paint_over_jpeg_quality);
  newRenderable.videoCRF = isSettingRenderable(s.video_crf);
  newRenderable.videoPaintoverCRF = isSettingRenderable(s.video_paintover_crf);
  newRenderable.videoPaintoverBurstFrames = isSettingRenderable(s.video_paintover_burst_frames);
  newRenderable.usePaintOverQuality = isSettingRenderable(s.use_paint_over_quality);
  newRenderable.videoStreamingMode = isSettingRenderable(s.video_streaming_mode);
  newRenderable.videoFullColor = isSettingRenderable(s.video_fullcolor);
  newRenderable.useCpu = isSettingRenderable(s.use_cpu);
  newRenderable.uiScaling = isSettingRenderable(s.scaling_dpi);
  newRenderable.binaryClipboard = isSettingRenderable(s.enable_binary_clipboard)
    && (s.clipboard_enabled?.value ?? true);
  newRenderable.useBrowserCursors = isSettingRenderable(s.use_browser_cursors);
  newRenderable.videoBitrate = isSettingRenderable(s.video_bitrate);
  newRenderable.audioBitrate = isSettingRenderable(s.audio_bitrate);
  // The HiDPI toggle drives use_css_scaling, inverted.
  newRenderable.hidpi = isSettingRenderable(s.use_css_scaling);
  newRenderable.forceAlignedResolution = isSettingRenderable(s.force_aligned_resolution);

  newRenderable.enableSharing = s.enable_sharing?.value ?? true;
  newRenderable.enableShared = s.enable_shared?.value ?? true;
  newRenderable.enablePlayer2 = s.enable_player2?.value ?? true;
  newRenderable.enablePlayer3 = s.enable_player3?.value ?? true;
  newRenderable.enablePlayer4 = s.enable_player4?.value ?? true;
  newRenderable.enableDualMode = s.enable_dual_mode?.value ?? false;

  // There is no server-side video on/off setting.
  newRenderable.videoToggle = true;
  newRenderable.audioToggle = isSettingRenderable(s.audio_enabled);
  newRenderable.microphoneToggle = isSettingRenderable(s.microphone_enabled);
  newRenderable.webcamToggle = isSettingRenderable(s.webcam_enabled)
    && (s.ui_sidebar_show_webcam?.value ?? true);
  newRenderable.gamepadToggle = isSettingRenderable(s.gamepad_enabled);

  newRenderable.enableRateControl = s.enable_rate_control?.value ?? true;
  const ftValue = s.file_transfers?.value;
  newRenderable.fileUpload = ftValue !== undefined ? ftValue.includes('upload') : true;
  newRenderable.fileDownload = ftValue !== undefined ? ftValue.includes('download') : true;

  return newRenderable;
}
