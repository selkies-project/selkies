/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Which controls a dashboard draws, from the server's settings payload.
 *
 * One rule decides a setting's control: it is drawn only while the user can
 * change it, so a locked setting, one whose menu an operator narrowed to a
 * single value, and a range pinned to one number all hide theirs. The
 * `ui_*` settings hide whole sections and buttons regardless, and a feature
 * the server refuses (a clipboard direction, a file-transfer direction, a
 * sharing role) is not offered rather than offered dead. Both shells read one
 * projection, so a gate wired on one is wired on the other.
 * @module
 */

import { isMacDesktop } from './util.js';

/**
 * Whether a control is worth rendering: not locked, with more than one
 * permitted value, and not a range pinned to one number.
 * @param {({locked?: boolean, allowed?: Array, min?: number, max?: number}|undefined)} setting
 *     The setting's entry in the payload; a missing entry renders.
 * @returns {boolean}
 */
export function isSettingRenderable(setting) {
  if (!setting) return true;
  if (setting.locked === true) return false;
  if (setting.allowed && setting.allowed.length <= 1) return false;
  if (setting.min !== undefined && setting.max !== undefined && setting.min === setting.max) return false;
  return true;
}

/** A payload entry's boolean `value`, or `fallback` without one. */
const flag = (entry, fallback) => (entry && entry.value !== undefined ? entry.value : fallback);

/**
 * Derives every visibility flag the panels read from a `serverSettings`
 * payload: the operator's `ui_*` toggles, per-control renderability from each
 * setting's own constraints, the sharing roles, the stream switches, and
 * the file-transfer directions.
 * @param {object|null} serverSettings The `serverSettings` payload.
 * @returns {Object<string, boolean>} Empty before the payload arrives, so a
 *     missing flag reads as shown.
 */
export function computeRenderableSettings(serverSettings) {
  if (!serverSettings) return {};
  const s = serverSettings;
  const r = {};

  r.videoSettings = flag(s.ui_sidebar_show_video_settings, true);
  r.audioSettings = flag(s.ui_sidebar_show_audio_settings, true);
  r.screenSettings = flag(s.ui_sidebar_show_screen_settings, true);
  r.stats = flag(s.ui_sidebar_show_stats, true);
  r.clipboard = flag(s.ui_sidebar_show_clipboard, true) && flag(s.clipboard_enabled, true);
  r.files = flag(s.ui_sidebar_show_files, true);
  r.apps = flag(s.ui_sidebar_show_apps, true);
  r.sharing = flag(s.ui_sidebar_show_sharing, true) && flag(s.enable_sharing, true);
  r.fullscreen = flag(s.ui_sidebar_show_fullscreen, true);
  r.gamingMode = flag(s.ui_sidebar_show_gaming_mode, true);
  r.trackpad = flag(s.ui_sidebar_show_trackpad, true);
  r.keyboardButton = flag(s.ui_sidebar_show_keyboard_button, true);
  r.softButtons = flag(s.ui_sidebar_show_soft_buttons, true);
  r.coreButtons = flag(s.ui_show_core_buttons, true);
  r.shortcuts = flag(s.ui_sidebar_show_shortcuts, true);
  // Hides the gamepad preview and its switches only, never the gamepad input toggle.
  r.gamepads = flag(s.ui_sidebar_show_gamepads, true);
  r.connectionIndicator = flag(s.ui_show_connection_indicator, true);
  r.sidebar = flag(s.ui_show_sidebar, true);

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
  r.uiScaling = isSettingRenderable(s.scaling_dpi);
  r.binaryClipboard = isSettingRenderable(s.enable_binary_clipboard) && flag(s.clipboard_enabled, true);
  // A direction the server refuses is not offered rather than offered dead.
  r.clipboardUp = flag(s.clipboard_in_enabled, true);
  r.clipboardDown = flag(s.clipboard_out_enabled, true);
  r.clipboardSeamless = isSettingRenderable(s.clipboard_seamless) && flag(s.clipboard_enabled, true);
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

  r.enableSharing = flag(s.enable_sharing, true);
  r.enableShared = flag(s.enable_shared, true);
  r.enablePlayer2 = flag(s.enable_player2, true);
  r.enablePlayer3 = flag(s.enable_player3, true);
  r.enablePlayer4 = flag(s.enable_player4, true);
  r.enableDualMode = flag(s.enable_dual_mode, false);

  // There is no server-side video on/off setting.
  r.videoToggle = true;
  r.audioToggle = isSettingRenderable(s.audio_enabled);
  r.microphoneToggle = isSettingRenderable(s.microphone_enabled);
  r.webcamToggle = isSettingRenderable(s.webcam_enabled) && flag(s.ui_sidebar_show_webcam, true);
  r.webcamEncoder = isSettingRenderable(s.webcam_encoder) && r.webcamToggle;
  r.gamepadToggle = isSettingRenderable(s.gamepad_enabled);

  r.enableRateControl = flag(s.enable_rate_control, true);
  const transfers = s.file_transfers && s.file_transfers.value;
  r.fileUpload = transfers !== undefined ? transfers.includes('upload') : true;
  r.fileDownload = transfers !== undefined ? transfers.includes('download') : true;

  return r;
}
