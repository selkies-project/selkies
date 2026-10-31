/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Selkies-specific utilities shared by the dashboard's panels and layouts.
 *
 * Storage keys: every localStorage key is prefixed with the storage namespace
 * the streaming cores derive from the page URL (`lib/util.js`), and the
 * per-display settings gain a `_display2` suffix on the secondary display,
 * selected by the `#display2` URL hash. The route prefix is the cores'
 * derivation too, so the dashboard and both cores agree on all of them.
 *
 * Core state cache: the core broadcasts `serverSettings` once per connection
 * and `clipboardContentUpdate`, `effectiveCursorState`, and
 * `audioDeviceSelected` only when something changes, but a panel mounts when
 * its menu or section opens, after those messages. The latest of each is
 * cached at module scope so a late mount can seed its state synchronously;
 * the panels' own listeners still pick up later messages.
 * @module
 */

import { getRoutePrefix, getStorageAppName, isMobileClient, isMacDesktop } from "../../selkies-web-core/lib/util.js";
import { urlFragmentKeyword } from "../../selkies-web-core/lib/page-url.js";
import { hardwareKeyboard } from "../../selkies-web-core/lib/hardware-keyboard.js";

export { isMobileClient, hardwareKeyboard, getRoutePrefix, getStorageAppName };

/**
 * Union of both streaming cores' `PER_DISPLAY_SETTINGS` lists so the
 * dashboard and whichever core is running agree on which keys get the
 * `_display2` suffix. The websockets core alone owns `jpeg_quality` and
 * `paint_over_jpeg_quality` (jpeg is websockets framing); a key the running
 * core ignores is inert, while a missing one would make the secondary display
 * write the primary's key.
 */
const PER_DISPLAY_SETTINGS = [
  'framerate', 'video_crf', 'video_fullcolor', 'video_10bit',
  'video_streaming_mode', 'jpeg_quality', 'paint_over_jpeg_quality', 'use_cpu', 'cpu_encode_threading',
  'video_paintover_crf', 'video_paintover_burst_frames', 'use_paint_over_quality',
  'manual_resolution', 'manual_width', 'manual_height',
  'encoder', 'scaleLocallyManual', 'use_browser_cursors', 'rate_control_mode',
  'video_bitrate', 'force_aligned_resolution', 'scaling_dpi',
];

const urlHash = typeof window !== 'undefined' ? urlFragmentKeyword() : '';
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

const storageAppName = getStorageAppName();

/** The localStorage key for a setting on this display. */
export function getPrefixedKey(key: string): string {
  const prefixedKey = `${storageAppName}_${key}`;
  if (displayId === 'display2' && PER_DISPLAY_SETTINGS.includes(key)) {
    return `${prefixedKey}_display2`;
  }
  return prefixedKey;
}

/** Reads a stored boolean, `fallback` where nothing is stored. */
export function storedBool(key: string, fallback: boolean): boolean {
  const saved = localStorage.getItem(getPrefixedKey(key));
  return saved !== null ? saved === 'true' : fallback;
}

/** The server's description of one setting in the `serverSettings` payload. */
export interface ServerSetting {
  value?: unknown;
  locked?: boolean;
  overridden?: boolean;
  allowed?: string[];
  min?: number;
  max?: number;
  default?: unknown;
}

/** The `serverSettings` payload: untyped JSON keyed by setting name. */
export type ServerSettings = Record<string, any>;

/** A document printed in the session, as the core hands it over: a blob URL. */
export interface PrintJob { name: string; url: string }

/** The server clipboard preview; a secret arrives as the flag alone, never its text. */
export interface ClipboardPreview { text: string; truncated: boolean; secret: boolean }

let lastServerSettings: ServerSettings | null = null;
let lastClipboardContent: ClipboardPreview | null = null;
let printJobs: PrintJob[] = [];
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
      lastClipboardContent = { text: message.text, truncated: message.truncated === true,
        secret: message.secret === true };
    } else if (message.type === 'effectiveCursorState' && typeof message.value === 'boolean') {
      lastEffectiveCursorState = message.value;
    } else if (message.type === 'audioDeviceSelected' && message.deviceId) {
      if (message.context === 'input') lastAudioDevices.input = message.deviceId;
      else if (message.context === 'output') lastAudioDevices.output = message.deviceId;
    } else if (message.type === 'printDocument' && typeof message.url === 'string') {
      printJobs = [...printJobs, { name: message.name, url: message.url }];
      window.dispatchEvent(new Event('printJobsChanged'));
    }
  });
}

/** The documents printed so far this page; `printJobsChanged` fires on each. */
export function getPrintJobs(): PrintJob[] {
  return printJobs;
}

/** The last `serverSettings` payload, or null before the first connection. */
export function getLastServerSettings(): ServerSettings | null {
  return lastServerSettings;
}

/** The last server clipboard preview; the core emits it only on clipboard events. */
export function getLastClipboardContent(): ClipboardPreview | null {
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
 * of the page; a remounted panel shows them again instead of pretending the
 * defaults are in use.
 */
export function getLastAudioDevices(): { input: string | null; output: string | null } {
  return lastAudioDevices;
}

/**
 * Whether a control is worth rendering: the user can actually change it,
 * meaning not locked and with more than one permitted value.
 */
export function isSettingRenderable(setting: ServerSetting | undefined): boolean {
  if (!setting) return true;
  if (setting.locked === true) return false;
  if (setting.allowed && setting.allowed.length <= 1) return false;
  if (setting.min !== undefined && setting.max !== undefined && setting.min === setting.max) return false;
  return true;
}

/** Every visibility flag the panels and layouts read, from `computeRenderableSettings`. */
export type Renderable = Record<string, boolean>;

/**
 * Derives every visibility flag the panels read from a `serverSettings`
 * payload: the admin's `ui_*` toggles, per-control renderability from each
 * setting's own constraints, the sharing roles, the stream-control menu
 * entries, and the file-transfer directions. Every flag defaults to shown
 * before the payload arrives, except those an absent server cannot offer.
 */
export function computeRenderableSettings(serverSettings: ServerSettings | null): Renderable {
  const s = serverSettings || {};
  const shown = (key: string) => s[key]?.value !== false;
  const r: Renderable = {};

  r.videoSettings = shown('ui_sidebar_show_video_settings');
  r.audioSettings = shown('ui_sidebar_show_audio_settings');
  r.screenSettings = shown('ui_sidebar_show_screen_settings');
  r.stats = shown('ui_sidebar_show_stats');
  r.clipboard = shown('ui_sidebar_show_clipboard') && shown('clipboard_enabled');
  r.files = shown('ui_sidebar_show_files');
  r.apps = shown('ui_sidebar_show_apps');
  r.sharing = shown('ui_sidebar_show_sharing') && shown('enable_sharing');
  r.fullscreen = shown('ui_sidebar_show_fullscreen');
  r.gamingMode = shown('ui_sidebar_show_gaming_mode');
  r.trackpad = shown('ui_sidebar_show_trackpad');
  r.keyboardButton = shown('ui_sidebar_show_keyboard_button');
  r.softButtons = shown('ui_sidebar_show_soft_buttons');
  r.coreButtons = shown('ui_show_core_buttons');
  r.shortcuts = shown('ui_sidebar_show_shortcuts');
  // Hides the gamepad preview only, never the gamepad input toggle.
  r.gamepads = shown('ui_sidebar_show_gamepads');
  r.chrome = shown('ui_show_sidebar');
  r.connectionIndicator = shown('ui_show_connection_indicator');

  r.encoder = isSettingRenderable(s.encoder);
  r.framerate = isSettingRenderable(s.framerate);
  r.jpegQuality = isSettingRenderable(s.jpeg_quality);
  r.paintOverJpegQuality = isSettingRenderable(s.paint_over_jpeg_quality);
  r.videoCRF = isSettingRenderable(s.video_crf);
  r.videoPaintoverCRF = isSettingRenderable(s.video_paintover_crf);
  r.videoPaintoverBurstFrames = isSettingRenderable(s.video_paintover_burst_frames);
  r.usePaintOverQuality = isSettingRenderable(s.use_paint_over_quality);
  r.videoStreamingMode = isSettingRenderable(s.video_streaming_mode);
  r.videoFullColor = isSettingRenderable(s.video_fullcolor);
  r.video10Bit = isSettingRenderable(s.video_10bit);
  r.useCpu = isSettingRenderable(s.use_cpu);
  r.cpuEncodeThreading = isSettingRenderable(s.cpu_encode_threading);
  r.uiScaling = isSettingRenderable(s.scaling_dpi);
  r.binaryClipboard = isSettingRenderable(s.enable_binary_clipboard) && shown('clipboard_enabled');
  // A direction the server refuses is not offered rather than offered dead.
  r.clipboardUp = shown('clipboard_in_enabled');
  r.clipboardDown = shown('clipboard_out_enabled');
  r.clipboardSeamless = isSettingRenderable(s.clipboard_seamless) && shown('clipboard_enabled');
  r.keyboardShortcuts = isSettingRenderable(s.keyboard_shortcuts);
  r.useBrowserCursors = isSettingRenderable(s.use_browser_cursors);
  r.rawPointerMotion = isSettingRenderable(s.raw_pointer_motion);
  // Offered only where Command exists to remap.
  r.macCmdAsCtrl = isSettingRenderable(s.mac_cmd_as_ctrl) && isMacDesktop();
  r.videoBitrate = isSettingRenderable(s.video_bitrate);
  r.audioBitrate = isSettingRenderable(s.audio_bitrate);
  // The HiDPI toggle drives use_css_scaling, inverted.
  r.hidpi = isSettingRenderable(s.use_css_scaling);
  r.forceAlignedResolution = isSettingRenderable(s.force_aligned_resolution);
  r.layout = isSettingRenderable(s.ui_layout);

  r.enableSharing = shown('enable_sharing');
  r.enableShared = shown('enable_shared');
  r.enablePlayer2 = shown('enable_player2');
  r.enablePlayer3 = shown('enable_player3');
  r.enablePlayer4 = shown('enable_player4');
  r.enableDualMode = s.enable_dual_mode?.value === true;

  // There is no server-side video on/off setting.
  r.videoToggle = true;
  r.audioToggle = isSettingRenderable(s.audio_enabled);
  r.microphoneToggle = isSettingRenderable(s.microphone_enabled);
  r.webcamToggle = isSettingRenderable(s.webcam_enabled) && shown('ui_sidebar_show_webcam');
  r.webcamEncoder = isSettingRenderable(s.webcam_encoder) && r.webcamToggle;
  r.gamepadToggle = isSettingRenderable(s.gamepad_enabled);

  r.enableRateControl = shown('enable_rate_control');
  const transfers: string[] | undefined = s.file_transfers?.value;
  r.fileUpload = transfers !== undefined ? transfers.includes('upload') : true;
  r.fileDownload = transfers !== undefined ? transfers.includes('download') : true;
  // second_screen is effective availability (admin flag and backend capacity).
  r.secondScreen = s.second_screen?.value === true;

  return r;
}

/**
 * The hover text of a labeled control: its label, then its detail after a
 * colon where it has one, so a label the row cut short still reads whole.
 */
export function hint(label: unknown, detail?: string): string | undefined {
  if (typeof label !== "string") return detail;
  return detail ? `${label}: ${detail}` : label;
}
