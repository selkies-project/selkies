/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Selkies-specific utilities shared by the dashboard components.
// Kept separate from src/lib/utils.ts which is managed by shadcn.

// The route prefix and the storage namespace are the same derivations the
// streaming cores use, so both dashboards and both cores agree on them.
import { getRoutePrefix, getStorageAppName } from "../../selkies-web-core/lib/util.js";

export { getRoutePrefix, getStorageAppName };

// --- Storage Key Prefixing ---

// Union of both streaming cores' PER_DISPLAY_SETTINGS lists so the dashboard
// and whichever core is running agree on which keys get the _display2 suffix.
// The websockets core alone owns 'jpeg_quality'/'paint_over_jpeg_quality'
// (jpeg is websockets framing); a key the running core ignores is inert, while
// a missing one would make the secondary display write the primary's key.
const PER_DISPLAY_SETTINGS = [
  'framerate', 'video_crf', 'video_fullcolor',
  'video_streaming_mode', 'jpeg_quality', 'paint_over_jpeg_quality', 'use_cpu',
  'video_paintover_crf', 'video_paintover_burst_frames', 'use_paint_over_quality',
  'is_manual_resolution_mode', 'manual_width', 'manual_height',
  'encoder', 'scaleLocallyManual', 'use_browser_cursors', 'rate_control_mode',
  'video_bitrate', 'force_aligned_resolution',
];

const urlHash = typeof window !== 'undefined' ? window.location.hash : '';
export const displayId = urlHash.startsWith('#display2') ? 'display2' : 'primary';
export const isSecondaryDisplay = displayId === 'display2';

/// Viewer-designated URL modes, known before the server answers with a role. The
/// control UI keys off this for its first render so a shared viewer never sees
/// controls it cannot use in the gap before `clientRoleUpdate` arrives.
export const isViewerUrlMode =
  urlHash.toLowerCase().startsWith('#shared') || /^#player[234]$/.test(urlHash.toLowerCase());

// Form factor is fixed for the life of the document, so it is resolved once
// and is available to the first render.
export const isMobileClient = typeof window !== 'undefined' && !!(
  (navigator as any).userAgentData?.mobile ||
  /Mobi|Android|iPhone|iPad|iPod|BlackBerry|IEMobile|Opera Mini/i.test(navigator.userAgent)
);

const storageAppName = getStorageAppName();

export function getPrefixedKey(key: string): string {
  const prefixedKey = `${storageAppName}_${key}`;
  if (displayId === 'display2' && PER_DISPLAY_SETTINGS.includes(key)) {
    return `${prefixedKey}_display2`;
  }
  return prefixedKey;
}

// --- Core state cache ---

// The core broadcasts serverSettings once per connection and the rest of the
// messages below only when something changes, but the panel components
// (Settings, Sharing, Files, Clipboard) mount lazily when their menu opens —
// after those messages. Cache the latest of each at module scope so a late
// mount can seed its state synchronously; the panels' own listeners still
// pick up later messages (reconnects, new clipboard events).
let lastServerSettings: any = null;
// clipboardContentUpdate: the server clipboard preview the Clipboard panel
// shows; the core emits it only on clipboard events.
let lastClipboardContent: { text: string; truncated: boolean } | null = null;
// effectiveCursorState: the cursor mode actually in effect (multi-monitor
// forces browser cursors on), emitted at connect / display-config time.
let lastEffectiveCursorState: boolean | null = null;
// audioDeviceSelected: the dashboard's own device picks, which the core keeps
// for the life of the page; a remounted Settings panel shows them again
// instead of pretending the defaults are in use.
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

export function getLastServerSettings(): any {
  return lastServerSettings;
}

export function getLastClipboardContent(): { text: string; truncated: boolean } | null {
  return lastClipboardContent;
}

export function getLastEffectiveCursorState(): boolean | null {
  return lastEffectiveCursorState;
}

export function getLastAudioDevices(): { input: string | null; output: string | null } {
  return lastAudioDevices;
}

// A control is worth rendering only when the user can actually change it:
// not locked, and with more than one permitted value.
export function isSettingRenderable(setting: any): boolean {
  if (!setting) return true;
  if (setting.locked === true) return false;
  if (setting.allowed && setting.allowed.length <= 1) return false;
  if (setting.min !== undefined && setting.max !== undefined && setting.min === setting.max) return false;
  return true;
}

export function computeRenderableSettings(serverSettings: any): Record<string, any> {
  if (!serverSettings) return {};
  const s = serverSettings;

  const newRenderable: Record<string, any> = {};

  // Admin-driven UI visibility toggles.
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
  // The floating gamepad card is wish's gamepads section (classic parity):
  // the flag hides the visualizer only, never the gamepad input toggle.
  newRenderable.gamepads = s.ui_sidebar_show_gamepads?.value ?? true;

  // Per-control renderability derived from the settings' own constraints.
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
  // The HiDPI toggle drives use_css_scaling (inverted); hide it when locked.
  newRenderable.hidpi = isSettingRenderable(s.use_css_scaling);
  newRenderable.forceAlignedResolution = isSettingRenderable(s.force_aligned_resolution);

  // Sharing roles.
  newRenderable.enableSharing = s.enable_sharing?.value ?? true;
  newRenderable.enableShared = s.enable_shared?.value ?? true;
  newRenderable.enablePlayer2 = s.enable_player2?.value ?? true;
  newRenderable.enablePlayer3 = s.enable_player3?.value ?? true;
  newRenderable.enablePlayer4 = s.enable_player4?.value ?? true;
  newRenderable.enableDualMode = s.enable_dual_mode?.value ?? false;

  // Stream-control menu entries. There is no server-side video on/off
  // setting, so the video toggle always renders.
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
