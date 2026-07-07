/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

// Selkies-specific utilities shared by the dashboard components.
// Kept separate from src/lib/utils.ts which is managed by shadcn.

// --- Route Prefix ---

// Directory the SPA is served from, without a trailing slash ('' at the
// server root). Lets fetches like `${getRoutePrefix()}/api/status` work when
// the server runs under a subfolder prefix.
export function getRoutePrefix(): string {
  const pathname = window.location.pathname;
  const dirPath = pathname.substring(0, pathname.lastIndexOf('/') + 1);
  return dirPath.replace(/\/$/, '');
}

// --- Storage Key Prefixing ---

// Must mirror the streaming core's PER_DISPLAY_SETTINGS list so the
// dashboard and core agree on which keys get the _display2 suffix.
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

export function getStorageAppName(): string {
  if (typeof window === 'undefined') return '';
  // Origin + pathname only (NOT the full URL): a per-session ?token=... must not mint
  // a new localStorage namespace each connect. Must match the cores' derivation.
  const urlForKey = window.location.origin + window.location.pathname;
  // Must match the streaming cores' prefix sanitizer ([._-] literal class, not
  // a [.-_] range) so dashboard and cores share one storage prefix.
  return urlForKey.replace(/[^a-zA-Z0-9._-]/g, '_');
}

const storageAppName = getStorageAppName();

export function getPrefixedKey(key: string): string {
  const prefixedKey = `${storageAppName}_${key}`;
  if (displayId === 'display2' && PER_DISPLAY_SETTINGS.includes(key)) {
    return `${prefixedKey}_display2`;
  }
  return prefixedKey;
}

// --- Server Settings ---

// The core broadcasts serverSettings once per connection, but panel
// components (Settings, Sharing, Files, Clipboard) mount lazily when their
// menu opens — after the broadcast. Cache the latest payload at module scope
// so late mounts can read it synchronously; their own listeners still pick
// up subsequent broadcasts (e.g. reconnects).
let lastServerSettings: any = null;
if (typeof window !== 'undefined') {
  window.addEventListener('message', (event: MessageEvent) => {
    if (event.origin !== window.location.origin) return;
    if (event.data?.type === 'serverSettings') {
      lastServerSettings = event.data.payload;
    }
  });
}

export function getLastServerSettings(): any {
  return lastServerSettings;
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

  // Per-control renderability derived from the settings' own constraints.
  newRenderable.encoder = isSettingRenderable(s.encoder);
  newRenderable.encoderRtc = isSettingRenderable(s.encoder_rtc);
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
  newRenderable.gamepadToggle = isSettingRenderable(s.gamepad_enabled)
    && (s.ui_sidebar_show_gamepads?.value ?? true);

  newRenderable.enableRateControl = s.enable_rate_control?.value ?? true;
  const ftValue = s.file_transfers?.value;
  newRenderable.fileUpload = ftValue !== undefined ? ftValue.includes('upload') : true;
  newRenderable.fileDownload = ftValue !== undefined ? ftValue.includes('download') : true;

  return newRenderable;
}
