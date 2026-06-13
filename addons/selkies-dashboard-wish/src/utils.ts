// Selkies-specific utilities.
// Kept separate from src/lib/utils.ts which is managed by shadcn.

// --- Route Prefix ---

export function getRoutePrefix(): string {
  const pathname = window.location.pathname;
  const dirPath = pathname.substring(0, pathname.lastIndexOf('/') + 1);
  return dirPath.replace(/\/$/, '');
}

// --- Storage Key Prefixing ---

const PER_DISPLAY_SETTINGS = [
  'video_bitrate', 'framerate', 'h264_crf', 'h264_fullcolor',
  'h264_streaming_mode', 'jpeg_quality', 'paint_over_jpeg_quality', 'use_cpu',
  'h264_paintover_crf', 'h264_paintover_burst_frames', 'use_paint_over_quality',
  'is_manual_resolution_mode', 'manual_width', 'manual_height',
  'encoder', 'scaleLocallyManual', 'use_browser_cursors', 'rate_control_mode',
  'stream_mode',
];

const urlHash = typeof window !== 'undefined' ? window.location.hash : '';
const displayId = urlHash.startsWith('#display2') ? 'display2' : 'primary';

export function getStorageAppName(): string {
  if (typeof window === 'undefined') return '';
  const urlForKey = window.location.href.split('#')[0];
  return urlForKey.replace(/[^a-zA-Z0-9.-_]/g, '_');
}

export function getPrefixedKey(key: string): string {
  const storageAppName = getStorageAppName();
  const prefixedKey = `${storageAppName}_${key}`;
  if (displayId === 'display2' && PER_DISPLAY_SETTINGS.includes(key)) {
    return `${prefixedKey}_display2`;
  }
  return prefixedKey;
}

// --- Server Settings ---

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

  newRenderable.videoSettings = s.ui_sidebar_show_video_settings?.value ?? true;
  newRenderable.audioSettings = s.ui_sidebar_show_audio_settings?.value ?? true;
  newRenderable.screenSettings = s.ui_sidebar_show_screen_settings?.value ?? true;
  newRenderable.stats = s.ui_sidebar_show_stats?.value ?? true;
  newRenderable.clipboard = s.ui_sidebar_show_clipboard?.value ?? true;
  newRenderable.files = s.ui_sidebar_show_files?.value ?? true;
  newRenderable.apps = s.ui_sidebar_show_apps?.value ?? true;
  newRenderable.sharing = s.ui_sidebar_show_sharing?.value ?? true;
  newRenderable.gamepads = s.ui_sidebar_show_gamepads?.value ?? true;
  newRenderable.fullscreen = s.ui_sidebar_show_fullscreen?.value ?? true;
  newRenderable.gamingMode = s.ui_sidebar_show_gaming_mode?.value ?? true;
  newRenderable.trackpad = s.ui_sidebar_show_trackpad?.value ?? true;
  newRenderable.keyboardButton = s.ui_sidebar_show_keyboard_button?.value ?? true;
  newRenderable.softButtons = s.ui_sidebar_show_soft_buttons?.value ?? true;
  newRenderable.coreButtons = s.ui_show_core_buttons?.value ?? true;

  newRenderable.encoder = isSettingRenderable(s.encoder);
  newRenderable.encoder_rtc = isSettingRenderable(s.encoder_rtc);
  newRenderable.framerate = isSettingRenderable(s.framerate);
  newRenderable.jpeg_quality = isSettingRenderable(s.jpeg_quality);
  newRenderable.paint_over_jpeg_quality = isSettingRenderable(s.paint_over_jpeg_quality);
  newRenderable.h264_crf = isSettingRenderable(s.h264_crf);
  newRenderable.h264PaintoverCRF = isSettingRenderable(s.h264_paintover_crf);
  newRenderable.usePaintOverQuality = isSettingRenderable(s.use_paint_over_quality);
  newRenderable.h264StreamingMode = isSettingRenderable(s.h264_streaming_mode);
  newRenderable.h264FullColor = isSettingRenderable(s.h264_fullcolor);
  newRenderable.use_cpu = isSettingRenderable(s.use_cpu);
  newRenderable.uiScaling = isSettingRenderable(s.scaling_dpi);
  newRenderable.binaryClipboard = isSettingRenderable(s.enable_binary_clipboard);
  newRenderable.use_browser_cursors = isSettingRenderable(s.use_browser_cursors);
  newRenderable.video_bitrate = isSettingRenderable(s.video_bitrate);
  newRenderable.audio_bitrate = isSettingRenderable(s.audio_bitrate);

  const hypotheticalHidpi = s.hidpi_enabled || { value: true, locked: false };
  newRenderable.hidpi = hypotheticalHidpi.locked !== true;

  newRenderable.enableSharing = s.enable_sharing?.value ?? true;
  newRenderable.enableShared = s.enable_shared?.value ?? true;
  newRenderable.enablePlayer2 = s.enable_player2?.value ?? true;
  newRenderable.enablePlayer3 = s.enable_player3?.value ?? true;
  newRenderable.enablePlayer4 = s.enable_player4?.value ?? true;
  newRenderable.enableDualMode = s.enable_dual_mode?.value ?? false;

  newRenderable.videoToggle = isSettingRenderable(s.video_enabled);
  newRenderable.audioToggle = isSettingRenderable(s.audio_enabled);
  newRenderable.microphoneToggle = isSettingRenderable(s.microphone_enabled);
  newRenderable.gamepadToggle = isSettingRenderable(s.gamepad_enabled);

  newRenderable.enableRateControl = s.enable_rate_control?.value ?? false;
  const ftSetting = s.file_transfers;
  newRenderable.fileUpload = ftSetting ? ftSetting.value.includes('upload') : true;
  newRenderable.fileDownload = ftSetting ? ftSetting.value.includes('download') : true;

  return newRenderable;
}
