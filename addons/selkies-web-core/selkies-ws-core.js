/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import {
  GamepadManager
} from './lib/gamepad.js';
import {
  Input
} from './lib/input.js';

// Parse an audio frame body into the ordered Opus frames to decode, using RED redundancy
// to recover frames the sender dropped under backpressure (pcmflux's delivery ring and the
// server's audio queue both drop-oldest, and a dropped frame rides along as redundancy in
// the next packet). n_red==0 is the plain path: [0x01,0x00]+opus. n_red>0 is
// [0x01, n_red, pts32] + n_red*(4-byte header) + 1-byte primary header + block datas
// (redundant oldest-first, then primary); each block's timestamp is pts - tsOffset. Each
// frame is decoded at most once, in order: any block newer than the last one already
// played is taken, so a redundant copy fills the gap left by a dropped primary.
let lastAudioTs = null;
function audioTsNewer(a, b) {
  // 32-bit wrap-safe: true if a is strictly newer than b.
  const d = (a - b) >>> 0;
  return d !== 0 && d < 0x80000000;
}
function extractOpusFrames(arrayBuffer) {
  const bytes = new Uint8Array(arrayBuffer);
  const nRed = bytes[1];
  if (!nRed) { lastAudioTs = null; return [arrayBuffer.slice(2)]; }
  if (arrayBuffer.byteLength < 6 + nRed * 4 + 1) return [arrayBuffer.slice(2)]; // malformed
  const pts = ((bytes[2] << 24) | (bytes[3] << 16) | (bytes[4] << 8) | bytes[5]) >>> 0;
  let pos = 6;
  const offsets = [], lens = [];
  for (let i = 0; i < nRed; i++) {
    const field = (bytes[pos + 1] << 16) | (bytes[pos + 2] << 8) | bytes[pos + 3];
    offsets.push((field >> 10) & 0x3fff);
    lens.push(field & 0x3ff);
    pos += 4;
  }
  pos += 1; // primary header
  const blocks = [];
  for (let i = 0; i < nRed; i++) {
    blocks.push({ ts: (pts - offsets[i]) >>> 0, buf: arrayBuffer.slice(pos, pos + lens[i]) });
    pos += lens[i];
  }
  blocks.push({ ts: pts, buf: arrayBuffer.slice(pos) });
  if (lastAudioTs === null) {
    // First RED frame: anchor on the primary; don't replay its trailing redundancy.
    lastAudioTs = pts;
    return [blocks[blocks.length - 1].buf];
  }
  const out = [];
  let last = lastAudioTs;
  for (const b of blocks) {
    if (audioTsNewer(b.ts, last)) { out.push(b.buf); last = b.ts; }
  }
  lastAudioTs = last;
  return out;
}

export default function websockets() {
let decoder;
// Main decoder's current codec + coded dims; reconfigured when a keyframe's SPS
// reports a different profile/level (only when the codec actually changes).
let configuredMainCodec = null;
let mainDecoderCodedWidth = 0;
let mainDecoderCodedHeight = 0;
let isSidebarOpen = false;
let isSecondaryDisplayConnected = false;
let audioDecoderWorker = null;
let canvas = null;
let canvasContext = null;
let websocket;
let clientMode = null;
let clientRole = null;
let clientSlot = null;
let isTokenAuthMode = false;
let audioContext;
let audioWorkletNode;
let audioGainNode;
let currentVolume = 1.0;
let audioWorkletProcessorPort;
window.currentAudioBufferSize = 0;
// Concealment observability: zero-filled underrun samples + drop-oldest events reported by
// the playback AudioWorklet, and the main-thread >=N-packet drop-gate hits. Surfaced so the
// RED before/after acceptance metric is measurable.
window.currentAudioUnderrunSamples = 0;
window.currentAudioWorkletDropped = 0;
window.currentAudioDropped = 0;
let videoFrameBuffer = [];
// Adaptive paint cushion. Presenting only the newest decoded frame is latency-optimal,
// but on jittery decoders (Firefox software H.264) every slightly-late frame becomes a
// visible repeated-frame stall. Instead of paying a permanent one-frame latency tax, the
// cushion stays 0 while arrivals are healthy and rises to 1 only after an actual
// underrun (a paint tick that found nothing to paint mid-stream), decaying back after a
// stall-free period. Chrome-class decoders therefore keep minimal latency.
const VIDEO_CUSHION_HOLD_MS = 2000;
let lastVideoUnderrunTime = -VIDEO_CUSHION_HOLD_MS; // no cushion until a real underrun
let videoPaintedSinceLastTick = false;
// Diagnostics: how often arrivals underran the painter and whether the cushion is
// currently held (readable from the console / tests).
window.selkiesVideoStats = { underruns: 0, cushion: 0 };
// Track generators present decoded VideoFrames to a <video> element (GPU-composited,
// no per-frame 2D-canvas draw): MediaStreamTrackGenerator on the main thread (Chromium),
// or the standard worker-only VideoTrackGenerator whose track is transferred back here
// (Safari, and Firefox once it ships). Full-frame H.264 modes only; striped/JPEG modes
// and browsers with neither generator keep the canvas path.
let videoElement = null;
let videoFrameWriter = null;
let videoTrack = null;
let mstgActive = false;
let mstgLastGeom = null;
// Handoff gate: only hide the main canvas once the takeover sink has provably
// rendered a frame (requestVideoFrameCallback for a <video>, a one-time
// 'presented' message for the worker's OffscreenCanvas). Hiding it on the first
// write instead flashes black — the first track frame can arrive before the
// <video> starts rendering, and the worker draws its first frame asynchronously.
// sinkRevealGen invalidates stale rVFC callbacks across deactivate/re-activate.
let mstgRendered = false;
let videoWorkerRendered = false;
let sinkRevealGen = 0;
// Set true by the canvas-style writers (applyManualCanvasStyle / resetCanvasStyle /
// updateCanvasImageRendering); the present paths re-mirror the canvas box onto the
// <video>/worker canvas only when it's set, instead of reading+serializing cssText every frame.
let canvasGeomDirty = true;
let jpegStripeRenderQueue = [];
let triggerInitializeDecoder = () => {
  console.error("initializeDecoder function not yet assigned!");
};
let isVideoPipelineActive = true;
let isAudioPipelineActive = true;
let isMicrophoneActive = false;
let isGamepadEnabled;
let lastReceivedVideoFrameId = -1;
let mainDecoderHasKeyframe = false;
let pendingSharedKeyframe = null;
let initializationComplete = false;
let audioEnabled = true;
let microphoneEnabled = true;
// Display related resources
let displayId = 'primary';
let displayPosition = 'right';
const PER_DISPLAY_SETTINGS = [
    'framerate', 'video_crf', 'video_fullcolor',
    'video_streaming_mode', 'jpeg_quality', 'paint_over_jpeg_quality', 'use_cpu',
    'video_paintover_crf', 'video_paintover_burst_frames', 'use_paint_over_quality',
    'is_manual_resolution_mode', 'manual_width', 'manual_height',
    'encoder', 'scaleLocallyManual', 'use_browser_cursors', 'rate_control_mode',
    'video_bitrate', 'force_aligned_resolution'
];
// Microphone related resources
let micStream = null;
let micAudioContext = null;
let micSourceNode = null;
let micWorkletNode = null;
let micEncoder = null;
let micTimestampUs = 0;
let preferredInputDeviceId = null;
let preferredOutputDeviceId = null;
let metricsIntervalId = null;
let backpressureIntervalId = null;
let reconnectIntervalId = null;
// Watchdog for a lost START_VIDEO after the tab becomes visible again (the server
// never restarts encode -> black stream). Armed on visibilitychange->visible,
// cleared on the first VIDEO_STARTED / video chunk.
let startVideoWatchdogTimer = null;
let startVideoWatchdogAttempts = 0;
const START_VIDEO_WATCHDOG_MS = 3000;
const START_VIDEO_WATCHDOG_MAX_ATTEMPTS = 3;
const METRICS_INTERVAL_MS = 500;
const BACKPRESSURE_INTERVAL_MS = 50;
// Transport-capacity-derived chunk sizes: defaults assume aiohttp's stock 4 MiB
// receive cap; the server advertises its real ceiling (ws_max_message_bytes) in
// server_settings and these are recomputed to fill the frame.
let wsMaxMessageBytes = 4 * 1024 * 1024;
let UPLOAD_CHUNK_SIZE = wsMaxMessageBytes - 1024;           // binary frame + header margin
let CLIPBOARD_CHUNK_SIZE = ((wsMaxMessageBytes - 4096) * 3) >> 2; // raw bytes pre-base64
const applyWsMessageBudget = (bytes) => {
  if (!Number.isFinite(bytes) || bytes < 65536) return;
  wsMaxMessageBytes = bytes;
  UPLOAD_CHUNK_SIZE = wsMaxMessageBytes - 1024;
  CLIPBOARD_CHUNK_SIZE = ((wsMaxMessageBytes - 4096) * 3) >> 2;
};
const FILE_UPLOAD_THROTTLE_MS = 200;
let fileUploadProgressLastSent = {};
// Resources for resolution controls
window.is_manual_resolution_mode = false;
let manual_width = null;
let manual_height = null;
let originalWindowResizeHandler = null;
let handleResizeUI_globalRef = null;
let vncStripeDecoders = {};
let wakeLockSentinel = null;
let currentEncoderMode = 'h264enc-striped';
let useCssScaling = false;
let trackpadMode = false;
let scalingDPI = 96;
let antiAliasingEnabled = true;
let clipboard_in_enabled = true;
let clipboard_out_enabled = true;
let use_browser_cursors = false;
function applyEffectiveCursorSetting() {
    const userPreference = getBoolParam('use_browser_cursors', false);
    const isMultiMonitorActive = (displayId === 'display2' || (displayId === 'primary' && isSecondaryDisplayConnected));
    const finalSetting = isMultiMonitorActive ? true : userPreference;
    if (window.webrtcInput && typeof window.webrtcInput.setUseBrowserCursors === 'function') {
        console.log(`Applying effective cursor setting. Multi-monitor: ${isMultiMonitorActive}, User Pref: ${userPreference}, Final: ${finalSetting}`);
        window.webrtcInput.setUseBrowserCursors(finalSetting);
    }
}
function setRealViewportHeight() {
  const vh = window.innerHeight * 0.01;
  document.documentElement.style.setProperty('--vh', `${vh}px`);
}
// Base64-encode so the ',' and ':' wire delimiters survive in filenames.
function b64Path(p) {
  return btoa(unescape(encodeURIComponent(String(p))));
}
// One id per multipart clipboard transfer.
let clipboardTransferCounter = 0;
// Resources for clipboard
let enable_binary_clipboard = false;
// Latest text the server pushed; written to the local clipboard on the Ctrl/Cmd+C
// gesture for Safari/Firefox, which reject writes from the message handler. Kept
// as a fallback for when a freshly-requested copy times out (see below).
let lastServerClipboardText = '';
// Non-Chromium Ctrl/Cmd+C: REQUEST_CLIPBOARD, write a Promise into the ClipboardItem,
// and settle it from the next incoming clipboard message (fresh, not stale cache).
// Each entry: { resolve, mime }.
let pendingClipboardRequests = [];
// Last MIME type the server pushed (text/plain or an image/* type), so a binary
// copy advertises the correct type in the ClipboardItem.
let lastServerClipboardMime = 'text/plain';
// For binary copies: the most recent server image blob, returned by the copy Promise.
let lastServerClipboardBlob = null;
let multipartClipboard = {
    data: [],
    mimeType: '',
    totalSize: 0,
    receivedSize: 0,
    inProgress: false
};



let detectedSharedModeType = null;
let playerInputTargetIndex = 0;

const urlParams = new URLSearchParams(window.location.search);
const authToken = urlParams.get('token');

if (authToken) {
    isTokenAuthMode = true;
    console.log("Client is running in Token Authentication mode.");
} else {
    const hash = window.location.hash;
    if (hash === '#shared') {
        detectedSharedModeType = 'shared';
        playerInputTargetIndex = undefined;
    } else if (hash === '#player2') {
        detectedSharedModeType = 'player2';
        playerInputTargetIndex = 1;
    } else if (hash === '#player3') {
        detectedSharedModeType = 'player3';
        playerInputTargetIndex = 2;
    } else if (hash === '#player4') {
        detectedSharedModeType = 'player4';
        playerInputTargetIndex = 3;
    } else if (hash.startsWith('#display2')) {
        displayId = 'display2';
        const parts = hash.split('-');
        if (parts.length > 1) {
            const position = parts[1];
            if (['left', 'right', 'up', 'down'].includes(position)) {
                displayPosition = position;
            }
        }
    }
}
let sharedClientState = 'idle'; // Possible states: 'idle', 'ready', 'error'
let isSharedMode = detectedSharedModeType !== null;
// Whether the server will accept/execute 'cmd,' messages (mirrors the server's
// command_enabled setting). Default true so behavior is unchanged against older
// servers that never advertise the key; refreshed from each server_settings payload.
let serverCommandEnabled = true;
let sharedClientHasReceivedKeyframe = false;

if (isSharedMode) {
  console.log(`Client is running in ${detectedSharedModeType} mode.`);
}
if (displayId === 'display2') {
    console.log("Client is running in Secondary Display mode.");
}
window.onload = () => {
  'use strict';
};

// Set storage key based on URL
// Origin + pathname only (NOT the full URL): a per-session ?token=... must not mint a
// new localStorage namespace each connect. Must match selkies-core.js / selkies-wr-core.js.
const urlForKey = window.location.origin + window.location.pathname;
const storageAppName = urlForKey.replace(/[^a-zA-Z0-9._-]/g, '_');
// Guarded write: a full or unavailable store degrades to a warning instead of
// throwing QuotaExceededError into the caller.
const safeSetItem = (key, value) => {
  try {
    window.localStorage.setItem(key, value);
  } catch (e) {
    console.warn(`Selkies: could not persist '${key}' to localStorage:`, e);
  }
};

// Set page title
document.title = 'Selkies';
fetch('manifest.json')
  .then(response => response.json())
  .then(manifest => {
    if (manifest.name) {
      document.title = manifest.name;
    }
  })
  .catch(() => {
    // Pass
  });

let framerate = 60;
let video_crf = 25;
let video_fullcolor = false;
let video_streaming_mode = false;
let jpeg_quality = 60;
let paint_over_jpeg_quality = 90;
let use_cpu = false;
let video_paintover_crf = 18;
let video_paintover_burst_frames = 5;
let use_paint_over_quality = true;
let audio_bitrate = 320000;
let videoBitrate = 8;
let force_aligned_resolution = false;
let showStart = true;
let status = 'connecting';
let loadingText = '';
const gamepad = {
  gamepadState: 'disconnected',
  gamepadName: 'none',
};
const gpuStat = {
  gpuLoad: 0,
  gpuMemoryTotal: 0,
  gpuMemoryUsed: 0,
};
const cpuStat = {
  serverCPUUsage: 0,
  serverMemoryTotal: 0,
  serverMemoryUsed: 0,
};
const networkStat = {
  bandwidthMbps: 0,
  latencyMs: 0,
};
let debug = false;
let streamStarted = false;
let inputInitialized = false;
let scaleLocallyManual;
window.fps = 0;
let frameCount = 0;
let uniqueStripedFrameIdsThisPeriod = new Set();
let lastStripedFpsUpdateTime = performance.now();
let lastFpsUpdateTime = performance.now();
let statusDisplayElement;
let playButtonElement;
let overlayInput;
let rateControlMode = 'crf';

const getIntParam = (key, default_value) => {
  const prefixedKey = `${storageAppName}_${key}`;
  let finalKey = prefixedKey;
  if (displayId === 'display2' && PER_DISPLAY_SETTINGS.includes(key)) {
    finalKey = `${prefixedKey}_${displayId}`;
  }
  const value = window.localStorage.getItem(finalKey);
  return (value === null || value === undefined) ? default_value : parseInt(value);
};
// Fraction-preserving variant for values with sub-unit steps (Mbps bitrate).
const getFloatParam = (key, default_value) => {
  const prefixedKey = `${storageAppName}_${key}`;
  let finalKey = prefixedKey;
  if (displayId === 'display2' && PER_DISPLAY_SETTINGS.includes(key)) {
    finalKey = `${prefixedKey}_${displayId}`;
  }
  const value = window.localStorage.getItem(finalKey);
  const parsed = parseFloat(value);
  return (value === null || value === undefined || isNaN(parsed)) ? default_value : parsed;
};
const setIntParam = (key, value) => {
  const prefixedKey = `${storageAppName}_${key}`;
  let finalKey = prefixedKey;
  if (displayId === 'display2' && PER_DISPLAY_SETTINGS.includes(key)) {
    finalKey = `${prefixedKey}_${displayId}`;
  }
  if (value === null || value === undefined) {
    window.localStorage.removeItem(finalKey);
  } else {
    safeSetItem(finalKey, value.toString());
  }
};
const getBoolParam = (key, default_value) => {
  const prefixedKey = `${storageAppName}_${key}`;
  let finalKey = prefixedKey;
  if (displayId === 'display2' && PER_DISPLAY_SETTINGS.includes(key)) {
    finalKey = `${prefixedKey}_${displayId}`;
  }
  const v = window.localStorage.getItem(finalKey);
  if (v === null) {
    return default_value;
  }
  return v.toString().toLowerCase() === 'true';
};
const setBoolParam = (key, value) => {
  const prefixedKey = `${storageAppName}_${key}`;
  let finalKey = prefixedKey;
  if (displayId === 'display2' && PER_DISPLAY_SETTINGS.includes(key)) {
    finalKey = `${prefixedKey}_${displayId}`;
  }
  if (value === null || value === undefined) {
    window.localStorage.removeItem(finalKey);
  } else {
    safeSetItem(finalKey, value.toString());
  }
};
const getStringParam = (key, default_value) => {
  const prefixedKey = `${storageAppName}_${key}`;
  let finalKey = prefixedKey;
  if (displayId === 'display2' && PER_DISPLAY_SETTINGS.includes(key)) {
    finalKey = `${prefixedKey}_${displayId}`;
  }
  const value = window.localStorage.getItem(finalKey);
  return (value === null || value === undefined) ? default_value : value;
};
const setStringParam = (key, value) => {
  const prefixedKey = `${storageAppName}_${key}`;
  let finalKey = prefixedKey;
  if (displayId === 'display2' && PER_DISPLAY_SETTINGS.includes(key)) {
    finalKey = `${prefixedKey}_${displayId}`;
  }
  if (value === null || value === undefined) {
    window.localStorage.removeItem(finalKey);
  } else {
    safeSetItem(finalKey, value.toString());
  }
};
function sanitizeAndStoreSettings(serverSettings) {
  console.log("Sanitizing and storing settings based on server payload.");
  const changes = {};

  // Persist ONLY genuine user overrides. A server-pushed value with no stored
  // override is applied to the runtime (window[key]) but NOT written to
  // localStorage, so a later server-side change can still be re-pushed.
  // Persisting server defaults here left them stuck against future updates.
  const storageKeyFor = (key) => {
    const prefixedKey = `${storageAppName}_${key}`;
    return (displayId === 'display2' && PER_DISPLAY_SETTINGS.includes(key))
      ? `${prefixedKey}_${displayId}` : prefixedKey;
  };

  for (const key in serverSettings) {
    if (!serverSettings.hasOwnProperty(key)) continue;
    const setting = serverSettings[key];
    const finalKey = storageKeyFor(key);
    const wasUnset = window.localStorage.getItem(finalKey) === null;

    if (setting.min !== undefined && setting.max !== undefined) {
      const clientValue = getIntParam(key, setting.default);
      if (wasUnset) {
        window[key] = clientValue;
      } else if (clientValue < setting.min || clientValue > setting.max) {
        console.log(`Sanitizing '${key}': stored value ${clientValue} out of range [${setting.min}-${setting.max}]. Reverting to server default ${setting.default}.`);
        window.localStorage.removeItem(finalKey);
        window[key] = setting.default;
        changes[key] = setting.default;
      } else {
        window[key] = clientValue;
        setIntParam(key, clientValue);
      }
    }
    else if (setting.allowed !== undefined) {
      const isNumericEnum = !isNaN(parseFloat(setting.allowed[0]));
      const clientValueStr = isNumericEnum
        ? getIntParam(key, parseInt(setting.value, 10)).toString()
        : getStringParam(key, setting.value);
      const applyRuntime = (val) => { window[key] = isNumericEnum ? parseInt(val, 10) : val; };
      if (wasUnset) {
        applyRuntime(setting.value);
      } else if (!setting.allowed.includes(clientValueStr)) {
        console.log(`Sanitizing '${key}': stored "${clientValueStr}" not in allowed [${setting.allowed.join(', ')}]. Reverting to server default "${setting.value}".`);
        window.localStorage.removeItem(finalKey);
        applyRuntime(setting.value);
        changes[key] = setting.value;
      } else {
        applyRuntime(clientValueStr);
        if (isNumericEnum) setIntParam(key, parseInt(clientValueStr, 10));
        else setStringParam(key, clientValueStr);
      }
    }
    else if (typeof setting.value === 'boolean') {
      const serverValue = setting.value;
      if (setting.locked) {
        const clientValue = getBoolParam(key, !serverValue);
        if (clientValue !== serverValue) {
          console.log(`Sanitizing '${key}': setting is locked by server. Client value ${clientValue} is being overwritten with ${serverValue}.`);
          changes[key] = serverValue;
        }
        window[key] = serverValue;
        setBoolParam(key, serverValue);
      } else if (wasUnset) {
        window[key] = serverValue;
      } else {
        const clientValue = getBoolParam(key, serverValue);
        window[key] = clientValue;
        setBoolParam(key, clientValue);
      }
    }
    else if (setting.value !== undefined) {
      // Plain int/float/string settings (e.g. audio_channels): runtime-only —
      // they configure pipelines, not user preferences, so never persist.
      window[key] = setting.value;
    }
  }
  return changes;
}
framerate = getIntParam('framerate', framerate);
video_crf = getIntParam('video_crf', video_crf);
video_fullcolor = getBoolParam('video_fullcolor', video_fullcolor);
video_streaming_mode = getBoolParam('video_streaming_mode', video_streaming_mode);
jpeg_quality = getIntParam('jpeg_quality', jpeg_quality);
paint_over_jpeg_quality = getIntParam('paint_over_jpeg_quality', paint_over_jpeg_quality);
use_cpu = getBoolParam('use_cpu', use_cpu);
video_paintover_crf = getIntParam('video_paintover_crf', video_paintover_crf);
video_paintover_burst_frames = getIntParam('video_paintover_burst_frames', video_paintover_burst_frames);
use_paint_over_quality = getBoolParam('use_paint_over_quality', use_paint_over_quality);
audio_bitrate = getIntParam('audio_bitrate', audio_bitrate);
debug = getBoolParam('debug', debug);
currentEncoderMode = getStringParam('encoder', 'h264enc');
scaleLocallyManual = getBoolParam('scaleLocallyManual', true);
window.is_manual_resolution_mode = getBoolParam('is_manual_resolution_mode', false);
isGamepadEnabled = getBoolParam('isGamepadEnabled', true);
useCssScaling = getBoolParam('useCssScaling', false);
trackpadMode = getBoolParam('trackpadMode', false);
rateControlMode = getStringParam('rate_control_mode', rateControlMode);
videoBitrate = getFloatParam('video_bitrate', videoBitrate);
if (getStringParam('scaling_dpi', null) === null) {
  const dpr = window.devicePixelRatio || 1;
  const target = Math.round(dpr * 4) * 24;
  const presets = [120, 144, 168, 192, 216, 240, 288];
  scalingDPI = (dpr > 1 && presets.includes(target)) ? target : 96;
} else {
  scalingDPI = getIntParam('scaling_dpi', 96);
}
antiAliasingEnabled = getBoolParam('antiAliasingEnabled', true);
use_browser_cursors = getBoolParam('use_browser_cursors', false);
if (displayId === 'display2') {
    use_browser_cursors = true;
}
enable_binary_clipboard = getBoolParam('enable_binary_clipboard', enable_binary_clipboard);
clipboard_in_enabled = getBoolParam('clipboard_in_enabled', true);
clipboard_out_enabled = getBoolParam('clipboard_out_enabled', true);
force_aligned_resolution = getBoolParam('force_aligned_resolution', force_aligned_resolution);
// Init reads with fallbacks only and persists nothing: a fresh profile keeps every
// key unset so server-pushed defaults stay re-pushable. Only genuine user actions
// (and sanitizeAndStoreSettings for keys the user already overrode) write localStorage.

if (isSharedMode) {
    manual_width = 1280;
    manual_height = 720;
    console.log(`Shared mode: Initialized manual_width/Height to ${manual_width}x${manual_height}`);
} else {
    manual_width = getIntParam('manual_width', null);
    manual_height = getIntParam('manual_height', null);
}

const enterFullscreen = () => {
  if ('webrtcInput' in window && window.webrtcInput && typeof window.webrtcInput.enterFullscreen === 'function') {
    window.webrtcInput.enterFullscreen();
  }
};

const playStream = () => {
  showStart = false;
  if (playButtonElement) playButtonElement.classList.add('hidden');
  if (statusDisplayElement) statusDisplayElement.classList.add('hidden');
  requestWakeLock();
  console.log("playStream called in WebSocket mode - UI elements hidden.");
};

const enableClipboard = () => {
  navigator.clipboard
    .readText()
    .then((text) => {
      console.log("Clipboard API read access confirmed.");
    })
    .catch((err) => {
      console.error(`Failed to read clipboard contents: ${err}`);
    });
};

const updateStatusDisplay = () => {
  if (statusDisplayElement) {
    statusDisplayElement.textContent = loadingText || status;
  }
};

window.applyTimestamp = (msg) => {
  const now = new Date();
  const ts = `${now.getHours()}:${now.getMinutes()}:${now.getSeconds()}`;
  return `[${ts}] ${msg}`;
};

const alignResolution = (num) => {
  const alignment = force_aligned_resolution ? 16 : 2;
  return Math.floor(num / alignment) * alignment;
};

const isChromium = (() => {
  const isIOS = /iPad|iPhone|iPod/.test(navigator.userAgent) ||
                (navigator.platform === 'MacIntel' && navigator.maxTouchPoints > 1);
  const isFirefox = /Firefox|FxiOS/.test(navigator.userAgent);
  const isCriOS = /CriOS/.test(navigator.userAgent);
  const hasChromeObj = typeof window.chrome !== 'undefined';
  return hasChromeObj && !isIOS && !isFirefox && !isCriOS;
})();

// MediaStreamTrackGenerator is Chromium-only and exposed on Window (the main thread).
// The standard VideoTrackGenerator is exposed to a DedicatedWorker ONLY, so it is never
// defined here on the main thread (checking for it on Window is always false) -- it is
// detected and used inside the video worker instead. Sink priority is: worker-side
// VideoTrackGenerator (standard) > main-thread MediaStreamTrackGenerator (Chromium) >
// OffscreenCanvas worker (browsers with neither). No shipping browser exposes both a
// Window MSTG and a worker VTG, so when MSTG is present here we take it directly and skip
// the worker; revisit that short-circuit if one ever exposes both.
const supportsWindowMSTG = (typeof MediaStreamTrackGenerator !== 'undefined');

// Worker video sink for browsers without a main-thread generator. The same worker hosts
// either the standard VideoTrackGenerator (Safari, future Firefox) -- whose MediaStreamTrack
// is transferred back here for <video>.srcObject -- or, if that is unavailable (current
// Firefox), an OffscreenCanvas it composites onto. On by default; disable with
// ?offscreen_worker=false.
let USE_OFFSCREEN_WORKER = false;
let videoWorker = null;
let videoWorkerCanvas = null;
let videoWorkerActive = false;
let videoWorkerReady = false;
let videoWorkerMode = null;            // 'vtg' | 'canvas' | null (decided by the worker's self-probe)
let videoWorkerTrack = null;           // VTG track transferred from the worker (vtg mode)
let videoWorkerCanvasTransferred = false;
let videoWorkerLastGeom = null;
// Backpressure: cap frames in flight (worker acks each consumed frame); drop+close new
// frames while at the cap so GPU VideoFrames don't pile up and stall the decoder.
let videoWorkerInFlight = 0;
const VIDEO_WORKER_MAX_IN_FLIGHT = 3;
// Decode-in-worker: for non-shared Safari/Firefox full-frame H.264 ('h264enc'/'openh264enc'), the worker
// hosts the VideoDecoder so decode AND present stay off the main thread (no decoded frame
// crosses the boundary). Only the encoded bytes are transferred in. Tracks the last config
// pushed to the worker decoder; workerDecodeFailed sticks on a worker-decoder error so we
// fall back to main-thread decode (+ the worker sink, or the 2D canvas).
let decodeInWorker = false;
let workerDecoderCodec = null, workerDecoderW = 0, workerDecoderH = 0;
let workerDecodeFailed = false;
const VIDEO_WORKER_SRC = `
// Video sink + optional in-worker decoder. The sink is the standard worker-only
// VideoTrackGenerator (its MediaStreamTrack is transferred to the page for <video>.srcObject)
// or a transferred OffscreenCanvas. When the page sends encoded H.264 chunks the worker also
// DECODES them here, so decode and present stay off the main thread and no decoded frame ever
// crosses the thread boundary. A main-thread-decoded frame transferred in (m.frame) is still
// supported as a fallback during decoder warm-up.
let mode = null, oc = null, ctx = null, writer = null, closed = false, presented = false;
let dec = null, decKey = false, decNeedKey = false;
const OVERLOAD_QUEUE = 24;   // decode backlog (frames) that triggers a keyframe resync
const ack = () => self.postMessage({ ack: true });

// Present one decoded VideoFrame on the active sink. Consumes/closes the frame.
function present(f) {
  if (mode === 'vtg' && writer && !closed) {
    if (writer.desiredSize !== null && writer.desiredSize <= 0) { f.close(); return; }  // drop on sink backpressure
    // write() consumes/closes f on success; on reject (writable errored) it does NOT, so close it here to avoid leaking the frame.
    writer.write(f).catch(() => { try { f.close(); } catch (_) {} closed = true; self.postMessage({ type: 'error' }); });
    return;
  }
  try {
    if (ctx) {
      if (oc.width !== f.displayWidth || oc.height !== f.displayHeight) { oc.width = f.displayWidth; oc.height = f.displayHeight; }
      ctx.drawImage(f, 0, 0);
      // Tell the page the OffscreenCanvas has real content so it can hide the
      // main canvas (hiding it before this point flashes black).
      if (!presented) { presented = true; self.postMessage({ type: 'presented' }); }
    }
  } finally { f.close(); }
}

function closeDecoder() {
  if (dec) { try { if (dec.state !== 'closed') dec.close(); } catch (_) {} dec = null; }
  decKey = false; decNeedKey = false;
}

if (typeof VideoTrackGenerator !== 'undefined') {
  try {
    const g = new VideoTrackGenerator();
    writer = g.writable.getWriter();
    mode = 'vtg';
    self.postMessage({ type: 'mode', mode: 'vtg', track: g.track }, [g.track]);
  } catch (e) { self.postMessage({ type: 'mode', mode: 'canvas' }); }
} else {
  self.postMessage({ type: 'mode', mode: 'canvas' });
}

self.onmessage = (e) => {
  const m = e.data;
  if (m.canvas) { oc = m.canvas; ctx = oc.getContext('2d', { desynchronized: true }); if (!mode) mode = 'canvas'; return; }
  if (m.type === 'decoderConfig') {
    closeDecoder();
    try {
      dec = new VideoDecoder({ output: present, error: () => { closeDecoder(); self.postMessage({ type: 'decoderError' }); } });
      // configure() is synchronous (state becomes 'configured' immediately), so the next
      // chunk decodes without an async gap; an unsupported config surfaces via error().
      // No hardwareAcceleration hint: use the UA default so a hardware decoder is used
      // when available (much lower CPU on power-constrained clients); the pinned SPS
      // level keeps the hardware path from re-initializing mid-stream.
      dec.configure({ codec: m.codec, codedWidth: m.codedWidth, codedHeight: m.codedHeight, optimizeForLatency: true });
      decNeedKey = true;   // a keyframe is required after (re)configure
    } catch (err) { closeDecoder(); self.postMessage({ type: 'decoderError' }); }
    return;
  }
  if (m.type === 'closeDecoder') { closeDecoder(); return; }
  if (m.type === 'chunk') {
    if (!dec || dec.state !== 'configured') return;   // not ready yet; the page will resend a keyframe
    if (m.key) { decKey = true; decNeedKey = false; }
    else {
      if (!decKey || decNeedKey) { self.postMessage({ type: 'needKeyframe' }); return; }     // no usable keyframe yet
      if (dec.decodeQueueSize > OVERLOAD_QUEUE) { decNeedKey = true; self.postMessage({ type: 'needKeyframe' }); return; }  // decode falling behind -> resync
    }
    try { dec.decode(new EncodedVideoChunk({ type: m.key ? 'key' : 'delta', timestamp: m.timestamp, data: m.data })); }
    catch (err) { closeDecoder(); self.postMessage({ type: 'decoderError' }); }
    return;
  }
  if (m.frame) {   // fallback: a main-thread-decoded frame transferred in
    present(m.frame);
    ack();
  }
};`;

// Main-thread Chromium generator. VideoTrackGenerator is worker-only and is handled by the
// video worker, not here.
function createVideoTrackGenerator() {
  try {
    if (typeof MediaStreamTrackGenerator !== 'undefined') {       // Chromium, main thread
      const g = new MediaStreamTrackGenerator({ kind: 'video' });
      return { track: g, writable: g.writable };
    }
  } catch (e) {
    console.warn('MediaStreamTrackGenerator unavailable, using canvas:', e);
  }
  return null;
}

// Lazily wire the <video> element to a fresh generator. Returns true when ready.
function ensureMstgWriter() {
  if (videoFrameWriter) return true;
  if (!videoElement) return false;
  const gen = createVideoTrackGenerator();
  if (!gen) return false;
  videoTrack = gen.track;
  try { videoFrameWriter = gen.writable.getWriter(); }
  catch (e) { console.warn('track writer failed:', e); try { videoTrack.stop(); } catch (_) {} videoTrack = null; return false; }
  // If the writable errors/closes, fall back to the canvas so <video> doesn't freeze.
  if (videoFrameWriter.closed && videoFrameWriter.closed.catch) {
    const w = videoFrameWriter;
    videoFrameWriter.closed.catch(() => { if (videoFrameWriter === w) deactivateMstg(); });
  }
  try { videoElement.srcObject = new MediaStream([videoTrack]); }
  catch (e) {
    console.warn('srcObject failed:', e);
    try { videoFrameWriter.close(); } catch (_) {} videoFrameWriter = null;
    try { videoTrack.stop(); } catch (_) {} videoTrack = null;
    return false;
  }
  const p = videoElement.play(); if (p && p.catch) p.catch(() => {});
  return true;
}

function teardownMstgWriter() {
  if (videoFrameWriter) { try { videoFrameWriter.close(); } catch (e) {} videoFrameWriter = null; }
  if (videoTrack) { try { videoTrack.stop(); } catch (e) {} videoTrack = null; }
  if (videoElement) { try { videoElement.srcObject = null; } catch (e) {} }
}

// Send a VideoFrame to the track generator (shows <video>, hides canvas on first use).
// Returns true if consumed (caller must NOT close it); false to fall back to canvas.
function presentFrameToVideo(frame) {
  if (!ensureMstgWriter()) return false;
  if (!mstgActive) {
    mstgActive = true;
    mstgLastGeom = null; // force the box to be re-mirrored onto <video> below
    mstgRendered = false;
    if (videoElement) {
      videoElement.style.display = 'block';
      videoElement.style.objectFit = 'fill';
      if (typeof videoElement.requestVideoFrameCallback === 'function') {
        const gen = ++sinkRevealGen;
        videoElement.requestVideoFrameCallback(() => {
          if (gen !== sinkRevealGen || !mstgActive) return;
          mstgRendered = true;
          if (canvas) canvas.style.display = 'none';
        });
      } else {
        mstgRendered = true;   // can't observe rendering: keep the old behavior
      }
    }
  }
  // Resize handlers (resetCanvasStyle/applyManualCanvasStyle) re-show the canvas
  // with a fresh transform, so re-hide it every frame and re-mirror its box onto
  // the <video> whenever that geometry changes.
  if (canvas && videoElement) {
    if (mstgRendered && canvas.style.display !== 'none') canvas.style.display = 'none';
    // Re-mirror only when the canvas style changed (canvasGeomDirty) or on the first present
    // after activation (mstgLastGeom === null) -- avoids serializing cssText every frame.
    if (canvasGeomDirty || mstgLastGeom === null) {
      mstgLastGeom = canvas.style.cssText;
      videoElement.style.cssText = mstgLastGeom;
      videoElement.style.display = 'block';
      videoElement.style.objectFit = 'fill';
      canvasGeomDirty = false;
    }
  }
  // Until the <video> has rendered, also paint the frame on the canvas: a fresh
  // connection has nothing on the canvas yet, so hiding it (or showing an empty
  // <video>) would leave black until the sink's first rendered frame.
  if (!mstgRendered && canvas && canvasContext && canvas.width > 0 && canvas.height > 0) {
    try { canvasContext.drawImage(frame, 0, 0); } catch (e) {}
  }
  // Drop a frame if the sink can't keep up, to keep latency low.
  if (videoFrameWriter.desiredSize !== null && videoFrameWriter.desiredSize <= 0) {
    frame.close();
    return true;
  }
  const activeWriter = videoFrameWriter;
  videoFrameWriter.write(frame).catch(() => {
    try { frame.close(); } catch (e) {}
    // Rejected write = writable errored: tear down so the next frame falls back to canvas.
    if (videoFrameWriter === activeWriter) deactivateMstg();
  });
  return true;
}

// Lazily create the worker and complete the capability handshake. The worker self-probes
// VideoTrackGenerator on startup and reports its mode: 'vtg' (it transferred a track back
// for <video>.srcObject) or 'canvas' (we transfer it an OffscreenCanvas to composite on).
// Returns true once a sink is wired; until then frames fall back to the main canvas.
function ensureVideoWorker() {
  if (videoWorkerReady) return true;
  if (videoWorker) return false;   // created, handshake still in flight
  try {
    videoWorker = new Worker(URL.createObjectURL(new Blob([VIDEO_WORKER_SRC], { type: 'text/javascript' })));
    videoWorkerInFlight = 0;
    videoWorker.onerror = () => deactivateVideoWorker();
    videoWorker.onmessage = (e) => {
      const m = e.data;
      if (!m) return;
      if (m.ack) { if (videoWorkerInFlight > 0) videoWorkerInFlight--; return; }
      if (m.type === 'error') { deactivateVideoWorker(); return; }   // VTG writable errored
      if (m.type === 'presented') {                                  // worker canvas has real content now
        videoWorkerRendered = true;
        if (videoWorkerActive && canvas) canvas.style.display = 'none';
        return;
      }
      if (m.type === 'needKeyframe') { requestKeyframe(); return; }  // worker decoder needs a fresh keyframe
      if (m.type === 'decoderError') {
        // Worker-side decode failed: stop routing chunks to it and fall back to main-thread
        // decode. The worker sink (track/canvas) stays up to receive transferred frames.
        workerDecodeFailed = true;
        workerDecoderCodec = null; workerDecoderW = 0; workerDecoderH = 0;
        return;
      }
      if (m.type === 'mode') {
        if (m.mode === 'vtg' && m.track) {
          // Standard path: show the worker's track on the <video> element.
          if (!videoElement) { deactivateVideoWorker(); return; }
          videoWorkerMode = 'vtg';
          videoWorkerTrack = m.track;
          try {
            videoElement.srcObject = new MediaStream([m.track]);
            const p = videoElement.play(); if (p && p.catch) p.catch(() => {});
          } catch (err) { console.warn('VTG srcObject failed:', err); deactivateVideoWorker(); return; }
          videoWorkerReady = true;
        } else {
          // Fallback: hand the worker an OffscreenCanvas to composite on.
          videoWorkerMode = 'canvas';
          if (!videoWorkerCanvas) { deactivateVideoWorker(); return; }
          try {
            const off = videoWorkerCanvas.transferControlToOffscreen();
            videoWorkerCanvasTransferred = true;
            videoWorker.postMessage({ canvas: off }, [off]);
          } catch (err) { console.warn('OffscreenCanvas transfer failed:', err); deactivateVideoWorker(); return; }
          videoWorkerReady = true;
        }
      }
    };
    return false;   // not ready until the worker reports its mode
  } catch (e) {
    console.warn('video worker init failed, using main canvas:', e);
    deactivateVideoWorker();
    return false;
  }
}

function deactivateVideoWorker() {
  const wasVtg = (videoWorkerMode === 'vtg');
  const wasTransferred = videoWorkerCanvasTransferred;
  videoWorkerActive = false; videoWorkerReady = false; videoWorkerMode = null;
  videoWorkerInFlight = 0; videoWorkerCanvasTransferred = false;
  videoWorkerRendered = false; sinkRevealGen++;
  // Forget the worker decoder config so a freshly recreated worker gets (re)configured.
  workerDecoderCodec = null; workerDecoderW = 0; workerDecoderH = 0;
  if (videoWorker) { try { videoWorker.terminate(); } catch (_) {} videoWorker = null; }
  if (wasVtg) {
    if (videoWorkerTrack) { try { videoWorkerTrack.stop(); } catch (_) {} videoWorkerTrack = null; }
    if (videoElement) { try { videoElement.srcObject = null; } catch (_) {} videoElement.style.display = 'none'; }
  }
  if (wasTransferred && videoWorkerCanvas) {
    // The OffscreenCanvas was transferred to the (now-terminated) worker and can never
    // be transferred again, so swap in a fresh <canvas> — otherwise a later
    // ensureVideoWorker() would throw InvalidStateError on transferControlToOffscreen().
    const parent = videoWorkerCanvas.parentNode;
    const fresh = document.createElement('canvas');
    fresh.id = videoWorkerCanvas.id;
    fresh.style.display = 'none';
    if (parent) parent.replaceChild(fresh, videoWorkerCanvas);
    videoWorkerCanvas = fresh;
  } else if (videoWorkerCanvas) {
    videoWorkerCanvas.style.display = 'none';
  }
  if (canvas) canvas.style.display = 'block';
}

// Show the active worker sink (<video> for VTG, the worker canvas otherwise), hide the main
// canvas, and mirror its box onto the sink. Returns false if no sink target exists yet.
function activateWorkerSinkDisplay() {
  const target = (videoWorkerMode === 'vtg') ? videoElement : videoWorkerCanvas;
  if (!target) return false;
  if (!videoWorkerActive) {
    videoWorkerActive = true; videoWorkerLastGeom = null;
    videoWorkerRendered = false;
    target.style.display = 'block'; target.style.objectFit = 'fill';
    if (videoWorkerMode === 'vtg') {
      if (typeof target.requestVideoFrameCallback === 'function') {
        const gen = ++sinkRevealGen;
        target.requestVideoFrameCallback(() => {
          if (gen !== sinkRevealGen || !videoWorkerActive) return;
          videoWorkerRendered = true;
          if (canvas) canvas.style.display = 'none';
        });
      } else {
        videoWorkerRendered = true;   // can't observe rendering: keep the old behavior
      }
    }
    // canvas mode: revealed by the worker's one-time 'presented' message
  }
  if (canvas) {
    if (videoWorkerRendered && canvas.style.display !== 'none') canvas.style.display = 'none';
    // Re-mirror the canvas box onto the active sink only when it changed or on the first
    // present after activation -- avoids serializing cssText every frame.
    if (canvasGeomDirty || videoWorkerLastGeom === null) {
      videoWorkerLastGeom = canvas.style.cssText;
      target.style.cssText = videoWorkerLastGeom;
      target.style.display = 'block';
      target.style.objectFit = 'fill';
      canvasGeomDirty = false;
    }
  }
  return true;
}

// Transfer a VideoFrame to the worker sink (VTG <video> or OffscreenCanvas). Used as the
// fallback when the frame was decoded on the main thread (e.g. decoder warm-up). Returns
// true if consumed (caller must NOT close it).
function presentFrameToWorker(frame) {
  if (!ensureVideoWorker()) return false;
  if (!activateWorkerSinkDisplay()) return false;
  // Backpressure: if the worker hasn't drained enough acked frames, drop this one
  // rather than letting GPU VideoFrames pile up in the worker queue (decoder stall).
  // Return true (consumed) so the caller does NOT also push it to the rAF buffer.
  if (videoWorkerInFlight >= VIDEO_WORKER_MAX_IN_FLIGHT) {
    try { frame.close(); } catch (_) {}
    return true;
  }
  try {
    videoWorker.postMessage({ frame }, [frame]);
    videoWorkerInFlight++;
  }
  // postMessage threw (e.g. frame already detached, or worker gone): the frame is
  // now closed and must NOT be reused — report it as consumed (true) so the caller
  // doesn't push a closed frame into the rAF buffer. Subsequent frames fall back to
  // the canvas via deactivateVideoWorker().
  catch (e) { try { frame.close(); } catch (_) {} deactivateVideoWorker(); return true; }
  return true;
}

// Forward an encoded full-frame H.264 chunk to the worker's own decoder, which decodes and
// presents it entirely off the main thread (no decoded frame crosses the boundary). dataBuf
// is transferred. Returns true if handled there; false to fall back to main-thread decode.
function feedWorkerDecoder(isKey, dataBuf, w, h, codec) {
  if (workerDecodeFailed) return false;
  if (!ensureVideoWorker()) return false;            // worker still handshaking
  if (!activateWorkerSinkDisplay()) return false;
  // (Re)configure the worker decoder when the codec or coded dimensions change.
  if (codec !== workerDecoderCodec || w !== workerDecoderW || h !== workerDecoderH) {
    try { videoWorker.postMessage({ type: 'decoderConfig', codec: codec, codedWidth: w, codedHeight: h }); }
    catch (e) { return false; }
    workerDecoderCodec = codec; workerDecoderW = w; workerDecoderH = h;
    requestKeyframe();   // WebCodecs needs a keyframe right after (re)configure
  }
  try { videoWorker.postMessage({ type: 'chunk', key: isKey, data: dataBuf, timestamp: performance.now() * 1000 }, [dataBuf]); }
  catch (e) { return false; }
  return true;
}

// Switch back to the canvas (striped/JPEG mode, or fallback). Idempotent.
function deactivateMstg() {
  if (!mstgActive) return;
  mstgActive = false;
  mstgRendered = false; sinkRevealGen++;
  if (videoElement) videoElement.style.display = 'none';
  if (canvas) canvas.style.display = '';
  teardownMstgWriter();
}

const getDynamicH264Codec = (width, height, is444, fps) => {
  if (!isChromium) {
    return 'avc1.42E01E';
  }
  const effFps = (typeof fps === 'number' && fps > 0) ? fps : 60;
  const pixelsPerSecond = width * height * effFps;
  // Match NVENC's emitted profile_idc so the decoder doesn't reconfigure
  // mid-stream: High (0x64) for 4:2:0, High 4:4:4 (0xF4) for 4:4:4.
  const profile = is444 ? 'F400' : '6400';
  // Floor the level at 5.2 (0x34) to match the encoder's emitted level so the
  // decoder doesn't reconfigure level-only on the first keyframe.
  let level;
  if (pixelsPerSecond <= 3840 * 2160 * 60) {
    level = '34';
  } else if (pixelsPerSecond <= 7680 * 4320 * 30) {
    level = '3C';
  } else if (pixelsPerSecond <= 7680 * 4320 * 60) {
    level = '3D';
  } else {
    level = '3E';
  }
  return `avc1.${profile}${level}`;
};

// Parse the codec from the stream's actual SPS (Chromium WebCodecs) instead of
// guessing from width*height*fps: scan an Annex-B keyframe for the first SPS NAL
// and build "avc1.PPCCLL". Returns null if none found (caller uses the heuristic).
const parseAvcCodecFromAnnexB = (bytes) => {
  if (!bytes || bytes.length < 5) return null;
  const hex2 = (n) => n.toString(16).toUpperCase().padStart(2, '0');
  const n = bytes.length;
  let i = 0;
  while (i + 3 < n) {
    // Find a start code: 00 00 01 or 00 00 00 01.
    let startLen = 0;
    if (bytes[i] === 0 && bytes[i + 1] === 0 && bytes[i + 2] === 1) {
      startLen = 3;
    } else if (i + 4 < n && bytes[i] === 0 && bytes[i + 1] === 0 && bytes[i + 2] === 0 && bytes[i + 3] === 1) {
      startLen = 4;
    } else {
      i++;
      continue;
    }
    const nalStart = i + startLen;
    if (nalStart >= n) return null;
    const nalHeader = bytes[nalStart];
    // forbidden_zero_bit must be 0; nal_unit_type is the low 5 bits.
    const nalType = nalHeader & 0x1f;
    if ((nalHeader & 0x80) === 0 && nalType === 7) {
      // SPS RBSP starts right after the 1-byte NAL header. profile_idc,
      // constraint flags, and level_idc are the first three bytes and (because
      // profile_idc is always >= 66) never contain emulation-prevention bytes.
      if (nalStart + 3 < n) {
        const profileIdc = bytes[nalStart + 1];
        const constraintFlags = bytes[nalStart + 2];
        const levelIdc = bytes[nalStart + 3];
        return `avc1.${hex2(profileIdc)}${hex2(constraintFlags)}${hex2(levelIdc)}`;
      }
      return null;
    }
    i = nalStart; // skip past this start code and keep scanning for the SPS
  }
  return null;
};

// Chromium only: reconfigure the decoder if a keyframe's SPS profile/level differs
// from the current config. Returns true if reconfigured. The caller decodes that
// keyframe right after (WebCodecs requires a keyframe post-configure).
const maybeReconfigureMainDecoderFromSps = (keyframeBytes) => {
  if (!isChromium) return false;
  if (!decoder || decoder.state !== 'configured') return false;
  const spsCodec = parseAvcCodecFromAnnexB(keyframeBytes);
  if (!spsCodec || spsCodec === configuredMainCodec) return false;
  const w = mainDecoderCodedWidth, h = mainDecoderCodedHeight;
  if (!(w > 0 && h > 0)) return false;
  const newConfig = {
    codec: spsCodec,
    codedWidth: w,
    codedHeight: h,
    optimizeForLatency: true
  };
  try {
    decoder.configure(newConfig);
    console.log(`Main VideoDecoder reconfigured from SPS: ${configuredMainCodec} -> ${spsCodec}`);
    configuredMainCodec = spsCodec;
    return true;
  } catch (e) {
    console.warn('SPS-driven decoder reconfigure failed, keeping previous codec:', e);
    return false;
  }
};

const updateCanvasImageRendering = () => {
  if (!canvas) return;
  canvasGeomDirty = true;  // image-rendering is part of cssText -> re-mirror to <video>/worker
  if (!antiAliasingEnabled) {
    if (canvas.style.imageRendering !== 'pixelated') {
      console.log("Anti-aliasing disabled by setting. Forcing 'pixelated' rendering.");
      canvas.style.imageRendering = 'pixelated';
      canvas.style.setProperty('image-rendering', 'crisp-edges', '');
    }
    return;
  }
  const dpr = window.devicePixelRatio || 1;
  if (isSharedMode || window.is_manual_resolution_mode || (useCssScaling && dpr > 1)) {
    if (canvas.style.imageRendering !== 'auto') {
      console.log("Smoothing enabled for manual resolution, high-DPR scaling, or shared mode.");
      canvas.style.imageRendering = 'auto';
    }
  } else {
    if (canvas.style.imageRendering !== 'pixelated') {
      console.log("Setting canvas rendering to 'pixelated' for 1:1 display.");
      canvas.style.imageRendering = 'pixelated';
      canvas.style.setProperty('image-rendering', 'crisp-edges', '');
    }
  }
};

const injectCSS = () => {
  const style = document.createElement('style');
  style.textContent = `
body {
  font-family: sans-serif;
  margin: 0;
  padding: 0;
  overflow: hidden;
  background-color: #000;
  color: #fff;
}
#app {
  display: flex;
  flex-direction: column;
  height: calc(var(--vh, 1vh) * 100);
  width: 100%;
}
.video-container {
  flex-grow: 1;
  flex-shrink: 1;
  display: flex;
  flex-direction: column;
  justify-content: center;
  align-items: center;
  height: 100%;
  width: 100%;
  position: relative;
  overflow: hidden;
}
.video-container video,
.video-container canvas,
.video-container #overlayInput {
    position: absolute;
    top: 0;
    left: 0;
    width: 100%;
    height: 100%;
}
.video-container video {
  max-width: 100%;
  max-height: 100%;
  object-fit: contain;
  display: none;
}
.video-container #videoCanvas {
    z-index: 2;
    pointer-events: none;
    display: block;
}
.video-container #overlayInput {
    opacity: 0;
    z-index: 3;
    caret-color: transparent;
    background-color: transparent;
    color: transparent;
    pointer-events: auto;
    -webkit-user-select: none;
    border: none;
    outline: none;
    padding: 0;
    margin: 0;
}
.video-container #playButton {
  position: absolute;
  top: 50%;
  left: 50%;
  transform: translate(-50%, -50%);
  z-index: 10;
}
.hidden {
  display: none !important;
}
.video-container .status-bar {
  position: absolute;
  bottom: 0;
  left: 0;
  width: 100%;
  padding: 5px;
  background-color: rgba(0, 0, 0, 0.7);
  color: #fff;
  text-align: center;
  z-index: 5;
}
#playButton {
  padding: 15px 30px;
  font-size: 1.5em;
  cursor: pointer;
  background-color: rgba(0, 0, 0, 0.5);
  color: white;
  border: 1px solid rgba(255, 255, 255, 0.3);
  border-radius: 3px;
  backdrop-filter: blur(5px);
}
.video-container.shared-user-mode #overlayInput {
  cursor: default !important;
}
  `;
  document.head.appendChild(style);
};

function sendFullSettingsUpdateToServer(reason) {
    if (isSharedMode) return;
    if (websocket && websocket.readyState === WebSocket.OPEN) {
        const settingsToSend = getCurrentSettingsPayload();
        const settingsJson = JSON.stringify(settingsToSend);
        const message = `SETTINGS,${settingsJson}`;
        websocket.send(message);
        console.log(`[websockets] Sent full settings update. Reason: ${reason}`);
    } else {
        console.warn(`[websockets] Cannot send full settings update. Reason: ${reason}. WebSocket not open.`);
    }
}

function getCurrentSettingsPayload() {
    const settingsToSend = {};
    const dpr = useCssScaling ? 1 : (window.devicePixelRatio || 1);
    // Send only keys with a stored (user-set) value: hardcoded fallbacks here
    // would override server-configured defaults for every untouched setting.
    const hasStoredParam = (key) => {
        let finalKey = `${storageAppName}_${key}`;
        if (displayId === 'display2' && PER_DISPLAY_SETTINGS.includes(key)) {
            finalKey = `${finalKey}_${displayId}`;
        }
        return window.localStorage.getItem(finalKey) !== null;
    };
    const storedEntries = [
        ['framerate', () => getIntParam('framerate', 60)],
        ['video_crf', () => getIntParam('video_crf', 25)],
        ['encoder', () => getStringParam('encoder', 'h264enc')],
        ['is_manual_resolution_mode', () => getBoolParam('is_manual_resolution_mode', false)],
        ['audio_bitrate', () => getIntParam('audio_bitrate', 320000)],
        ['video_fullcolor', () => getBoolParam('video_fullcolor', false)],
        ['video_streaming_mode', () => getBoolParam('video_streaming_mode', false)],
        ['jpeg_quality', () => getIntParam('jpeg_quality', 60)],
        ['paint_over_jpeg_quality', () => getIntParam('paint_over_jpeg_quality', 90)],
        ['use_cpu', () => getBoolParam('use_cpu', false)],
        ['video_paintover_crf', () => getIntParam('video_paintover_crf', 18)],
        ['video_paintover_burst_frames', () => getIntParam('video_paintover_burst_frames', 5)],
        ['use_paint_over_quality', () => getBoolParam('use_paint_over_quality', true)],
        ['scaling_dpi', () => getIntParam('scaling_dpi', 96)],
        ['enable_binary_clipboard', () => getBoolParam('enable_binary_clipboard', false)],
        ['rate_control_mode', () => getStringParam('rate_control_mode', 'crf')],
        ['video_bitrate', () => getFloatParam('video_bitrate', 8)],
        ['force_aligned_resolution', () => getBoolParam('force_aligned_resolution', false)],
    ];
    for (const [key, read] of storedEntries) {
        if (hasStoredParam(key)) settingsToSend[key] = read();
    }
    if (window.is_manual_resolution_mode && manual_width != null && manual_height != null) {
        settingsToSend['is_manual_resolution_mode'] = true;
        settingsToSend['manual_width'] = alignResolution(manual_width);
        settingsToSend['manual_height'] = alignResolution(manual_height);
    } else {
        const videoContainer = document.querySelector('.video-container');
        const rect = videoContainer ? videoContainer.getBoundingClientRect() : { width: window.innerWidth, height: window.innerHeight };
        settingsToSend['is_manual_resolution_mode'] = false;
        
        let initW = alignResolution(rect.width * dpr);
        let initH = alignResolution(rect.height * dpr);
        if (initW > 4080) initW = 4080;
        if (initH > 4080) initH = 4080;

        settingsToSend['initialClientWidth'] = initW;
        settingsToSend['initialClientHeight'] = initH;
    }
    settingsToSend['useCssScaling'] = useCssScaling;
    settingsToSend['displayId'] = displayId;
    if (displayId === 'display2') {
        settingsToSend['displayPosition'] = displayPosition;
    }
    // Advertise audio-RED capability so the server enables Opus redundancy for this stream.
    settingsToSend['audioRedundancy'] = true;
    return settingsToSend;
}

function updateToggleButtonAppearance(buttonElement, isActive) {
  if (!buttonElement) return;
  let label = 'Unknown';
  if (buttonElement.id === 'videoToggleBtn') label = 'Video';
  else if (buttonElement.id === 'audioToggleBtn') label = 'Audio';
  else if (buttonElement.id === 'micToggleBtn') label = 'Microphone';
  else if (buttonElement.id === 'gamepadToggleBtn') label = 'Gamepad';
  if (isActive) {
    buttonElement.textContent = `${label}: ON`;
    buttonElement.classList.remove('inactive');
    buttonElement.classList.add('active');
  } else {
    buttonElement.textContent = `${label}: OFF`;
    buttonElement.classList.remove('active');
    buttonElement.classList.add('inactive');
  }
}

function sendResolutionToServer(width, height) {
  if (isSharedMode) {
    console.log("Shared mode: Resolution sending to server is blocked.");
    return;
  }

  let realWidth, realHeight;
  let dprUsed = 1;

  if (window.is_manual_resolution_mode) {
    realWidth = alignResolution(width);
    realHeight = alignResolution(height);
  } else {
    dprUsed = useCssScaling ? 1 : (window.devicePixelRatio || 1);
    realWidth = alignResolution(width * dprUsed);
    realHeight = alignResolution(height * dprUsed);
  }

  if (realWidth > 4080) realWidth = 4080;
  if (realHeight > 4080) realHeight = 4080;

  const resString = `${realWidth}x${realHeight}`;
  console.log(`Sending resolution to server: ${resString}, DisplayID: ${displayId}, Manual Mode: ${window.is_manual_resolution_mode}, Pixel Ratio Used: ${dprUsed}, useCssScaling: ${useCssScaling}`);

  if (websocket && websocket.readyState === WebSocket.OPEN) {
    websocket.send(`r,${resString},${displayId}`);
  } else {
    console.warn("Cannot send resolution via WebSocket: Connection not open.");
  }
}

function applyManualCanvasStyle(targetWidth, targetHeight, scaleToFit) {
  if (!canvas || !canvas.parentElement) {
    console.error("Cannot apply manual canvas style: Canvas or parent container not found.");
    return;
  }
  if (targetWidth <=0 || targetHeight <=0) {
    console.warn(`Cannot apply manual canvas style: Invalid target dimensions ${targetWidth}x${targetHeight}`);
    return;
  }
  canvasGeomDirty = true;  // canvas box changes below -> re-mirror onto the <video>/worker canvas

  const dpr = (isSharedMode || window.is_manual_resolution_mode || useCssScaling) ? 1 : (window.devicePixelRatio || 1);
  const internalBufferWidth = alignResolution(targetWidth * dpr);
  const internalBufferHeight = alignResolution(targetHeight * dpr);

  if (canvas.width !== internalBufferWidth || canvas.height !== internalBufferHeight) {
    canvas.width = internalBufferWidth;
    canvas.height = internalBufferHeight;
    console.log(`Canvas internal buffer set to: ${internalBufferWidth}x${internalBufferHeight}`);
  }
  const container = canvas.parentElement;
  const containerWidth = container.clientWidth;
  const containerHeight = container.clientHeight;

  let cssWidthStr, cssHeightStr, topStr, leftStr;

  if (scaleToFit) {
    const logicalAspectRatio = targetWidth / targetHeight;
    const containerAspectRatio = containerWidth / containerHeight;
    let cssWidth, cssHeight;
    if (logicalAspectRatio > containerAspectRatio) {
      cssWidth = containerWidth;
      cssHeight = containerWidth / logicalAspectRatio;
    } else {
      cssHeight = containerHeight;
      cssWidth = containerHeight * logicalAspectRatio;
    }
    const topOffset = (containerHeight - cssHeight) / 2;
    const leftOffset = (containerWidth - cssWidth) / 2;

    cssWidthStr = `${cssWidth}px`;
    cssHeightStr = `${cssHeight}px`;
    topStr = `${topOffset}px`;
    leftStr = `${leftOffset}px`;

    canvas.style.position = 'absolute';
    canvas.style.width = cssWidthStr;
    canvas.style.height = cssHeightStr;
    canvas.style.top = topStr;
    canvas.style.left = leftStr;
    canvas.style.objectFit = 'contain';
    console.log(`Applied manual style (Scaled): CSS ${cssWidth.toFixed(2)}x${cssHeight.toFixed(2)}, Buffer ${internalBufferWidth}x${internalBufferHeight}, Pos ${leftOffset.toFixed(2)},${topOffset.toFixed(2)}`);
  } else {
    cssWidthStr = `${targetWidth}px`;
    cssHeightStr = `${targetHeight}px`;
    const topOffset = (containerHeight - targetHeight) / 2;
    const leftOffset = (containerWidth - targetWidth) / 2;
    topStr = `${topOffset}px`;
    leftStr = `${leftOffset}px`;

    canvas.style.position = 'absolute';
    canvas.style.width = cssWidthStr;
    canvas.style.height = cssHeightStr;
    canvas.style.top = topStr;
    canvas.style.left = leftStr;
    canvas.style.objectFit = 'fill';
    console.log(`Applied manual style (Exact): CSS ${targetWidth}x${targetHeight}, Buffer ${internalBufferWidth}x${internalBufferHeight}, Pos ${leftOffset.toFixed(2)},${topOffset.toFixed(2)}`);
  }
  canvas.style.display = 'block';
  updateCanvasImageRendering();

  const overlayInputEl = document.getElementById('overlayInput');
  if (overlayInputEl) {
      overlayInputEl.style.position = 'absolute';
      overlayInputEl.style.width = cssWidthStr;
      overlayInputEl.style.height = cssHeightStr;
      overlayInputEl.style.top = topStr;
      overlayInputEl.style.left = leftStr;
  }
  if (window.webrtcInput && typeof window.webrtcInput.resize === 'function') {
      window.webrtcInput.resize();
  }
}

function resetCanvasStyle(streamWidth, streamHeight) {
  if (!canvas) return;
  if (streamWidth <= 0 || streamHeight <= 0) {
    console.warn(`Cannot reset canvas style: Invalid stream dimensions ${streamWidth}x${streamHeight}`);
    return;
  }
  // Geometry changed: the per-stripe-row keys (keyed by startY) are now stale, so drop them
  // to bound this map's growth across a session of resizes (JPEG stripe mode).
  lastDrawnJpegStripeFrameId = {};
  canvasGeomDirty = true;  // re-mirror the canvas box onto the <video>/worker canvas

  const dpr = useCssScaling ? 1 : (window.devicePixelRatio || 1); 
  const internalBufferWidth = alignResolution(streamWidth * dpr);
  const internalBufferHeight = alignResolution(streamHeight * dpr);

  if (canvas.width !== internalBufferWidth || canvas.height !== internalBufferHeight) {
    canvas.width = internalBufferWidth;
    canvas.height = internalBufferHeight;
    console.log(`Canvas internal buffer reset to: ${internalBufferWidth}x${internalBufferHeight}`);
  }

  const cssWidth = `${streamWidth}px`;
  const cssHeight = `${streamHeight}px`;

  canvas.style.width = cssWidth;
  canvas.style.height = cssHeight;

  const overlayInput = document.getElementById('overlayInput');
  if (overlayInput) {
      overlayInput.style.width = cssWidth;
      overlayInput.style.height = cssHeight;
      overlayInput.style.position = 'absolute';
  }

  const container = canvas.parentElement;
  if (container) {
    const containerWidth = container.clientWidth;
    const containerHeight = container.clientHeight;

    const leftOffset = Math.floor((containerWidth - streamWidth) / 2);
    const topOffset = Math.floor((containerHeight - streamHeight) / 2);

    canvas.style.position = 'absolute';
    canvas.style.top = `${topOffset}px`;
    canvas.style.left = `${leftOffset}px`;
    
    if (overlayInput) {
        overlayInput.style.top = `${topOffset}px`;
        overlayInput.style.left = `${leftOffset}px`;
    }

    console.log(`Reset canvas CSS to ${streamWidth}px x ${streamHeight}px, Pos ${leftOffset},${topOffset}, object-fit: fill. Buffer: ${internalBufferWidth}x${internalBufferHeight}`);
  } else {
    canvas.style.position = 'absolute';
    canvas.style.top = '0px';
    canvas.style.left = '0px';
    if (overlayInput) {
        overlayInput.style.top = '0px';
        overlayInput.style.left = '0px';
    }
    console.log(`Reset canvas CSS to ${streamWidth}px x ${streamHeight}px, Pos 0,0 (no parent metrics), object-fit: fill. Buffer: ${internalBufferWidth}x${internalBufferHeight}`);
  }

  canvas.style.objectFit = 'fill';
  canvas.style.display = 'block';
  updateCanvasImageRendering();

  if (window.webrtcInput && typeof window.webrtcInput.resize === 'function') {
      window.webrtcInput.resize();
  }
}

function enableAutoResize() {
  if (directManualLocalScalingHandler) {
    console.log("Switching to Auto Mode: Removing direct manual local scaling listener.");
    window.removeEventListener('resize', directManualLocalScalingHandler);
  }
  if (originalWindowResizeHandler) {
    console.log("Switching to Auto Mode: Adding original (auto) debounced resize listener.");
    window.removeEventListener('resize', originalWindowResizeHandler);
    window.addEventListener('resize', originalWindowResizeHandler);
    if (typeof handleResizeUI_globalRef === 'function') {
      console.log("Triggering immediate auto-resize calculation for auto mode.");
      handleResizeUI_globalRef();
    } else {
      console.warn("handleResizeUI function not directly callable from enableAutoResize. Auto-resize will occur on next event.");
    }
  } else {
    console.warn("Cannot enable auto-resize: originalWindowResizeHandler not found.");
  }
}

const directManualLocalScalingHandler = () => {
  if (window.is_manual_resolution_mode && !isSharedMode && manual_width != null && manual_height != null && manual_width > 0 && manual_height > 0) {
    applyManualCanvasStyle(manual_width, manual_height, scaleLocallyManual);
  }
};

function disableAutoResize() {
  if (originalWindowResizeHandler) {
    console.log("Switching to Manual Mode Local Scaling: Removing original (auto) resize listener.");
    window.removeEventListener('resize', originalWindowResizeHandler);
  }
  console.log("Switching to Manual Mode Local Scaling: Adding direct manual scaling listener.");
  window.removeEventListener('resize', directManualLocalScalingHandler);
  window.addEventListener('resize', directManualLocalScalingHandler);
  if (window.is_manual_resolution_mode && !isSharedMode && manual_width != null && manual_height != null && manual_width > 0 && manual_height > 0) {
    console.log("Applying current manual canvas style after enabling direct manual resize handler.");
    applyManualCanvasStyle(manual_width, manual_height, scaleLocallyManual);
  }
}

function updateUIForSharedMode() {
    if (!isSharedMode) return;

    const videoContainer = document.querySelector('.video-container');
    if (videoContainer) {
        videoContainer.classList.add('shared-user-mode');
        console.log("Shared mode: Added 'shared-user-mode' class to video container.");
    }

    const globalFileInput = document.getElementById('globalFileInput');
    if (globalFileInput) {
        globalFileInput.disabled = true;
        console.log("Shared mode: Disabled globalFileInput.");
    }
}


const initializeUI = () => {
  injectCSS();
  setRealViewportHeight();
  window.addEventListener('resize', setRealViewportHeight);
  window.addEventListener('requestFileUpload', handleRequestFileUpload);
  const appDiv = document.getElementById('app');
  if (!appDiv) {
    console.error("FATAL: Could not find #app element.");
    return;
  }
  const videoContainer = document.createElement('div');
  videoContainer.className = 'video-container';
  statusDisplayElement = document.createElement('div');
  statusDisplayElement.id = 'status-display';
  statusDisplayElement.className = 'status-bar';
  statusDisplayElement.textContent = 'Connecting...';
  videoContainer.appendChild(statusDisplayElement);
  overlayInput = document.createElement('input');
  overlayInput.type = 'search';
  overlayInput.readOnly = false;
  overlayInput.autocomplete = 'off';
  overlayInput.id = 'overlayInput';
  videoContainer.appendChild(overlayInput);

  canvas = document.getElementById('videoCanvas');
  if (!canvas) {
    canvas = document.createElement('canvas');
    canvas.id = 'videoCanvas';
  }
  videoContainer.appendChild(canvas);

  // Worker video sink for browsers without a main-thread generator (everything except
  // Chromium). The worker hosts VideoTrackGenerator when available, else an OffscreenCanvas.
  // The documented ?offscreen_worker=false URL param takes precedence over the
  // localStorage setting when present (getBoolParam only reads localStorage).
  const offscreenWorkerUrlParam = urlParams.get('offscreen_worker');
  const offscreenWorkerEnabled = (offscreenWorkerUrlParam !== null)
    ? (offscreenWorkerUrlParam.toLowerCase() === 'true')
    : getBoolParam('offscreen_worker', true);
  USE_OFFSCREEN_WORKER = !supportsWindowMSTG && offscreenWorkerEnabled;

  // Sibling <video> for either generator path (hidden until full-frame H.264 frames are
  // routed to it; the canvas stays the fallback): main-thread MSTG (Chromium) or a
  // VideoTrackGenerator track transferred out of the worker (Safari, future Firefox).
  if (supportsWindowMSTG || USE_OFFSCREEN_WORKER) {
    videoElement = document.getElementById('videoStream');
    if (!videoElement) {
      videoElement = document.createElement('video');
      videoElement.id = 'videoStream';
      videoElement.autoplay = true;
      videoElement.muted = true;
      videoElement.playsInline = true;
      videoElement.disableRemotePlayback = true;
    }
    videoElement.style.display = 'none';
    videoContainer.appendChild(videoElement);
  }

  // OffscreenCanvas the worker composites on when it has no VideoTrackGenerator (current
  // Firefox). Kept separate from the main canvas so the JPEG-stripe path is unaffected.
  if (USE_OFFSCREEN_WORKER) {
    videoWorkerCanvas = document.getElementById('videoWorkerCanvas');
    if (!videoWorkerCanvas) {
      videoWorkerCanvas = document.createElement('canvas');
      videoWorkerCanvas.id = 'videoWorkerCanvas';
    }
    videoWorkerCanvas.style.display = 'none';
    videoContainer.appendChild(videoWorkerCanvas);
  }

  // Decode full-frame H.264 inside the worker for non-shared browsers that use the worker
  // sink (Safari/Firefox): decode + present stay off the main thread. Shared mode and the
  // Chromium main-thread MSTG path keep main-thread decode. Kick the worker handshake now so
  // its decoder is ready before the first frame arrives.
  decodeInWorker = USE_OFFSCREEN_WORKER && !isSharedMode;
  if (decodeInWorker) ensureVideoWorker();

  if (isSharedMode) {
      if (!manual_width || manual_width <= 0 || !manual_height || manual_height <= 0) {
          manual_width = 1280; manual_height = 720;
      }
      applyManualCanvasStyle(manual_width, manual_height, true);
      window.addEventListener('resize', () => {
          if (isSharedMode && manual_width && manual_height && manual_width > 0 && manual_height > 0) {
              applyManualCanvasStyle(manual_width, manual_height, true);
          }
      });
      console.log(`Initialized UI in Shared Mode: Canvas buffer target ${manual_width}x${manual_height} (logical), will scale to fit viewport.`);
  } else if (is_manual_resolution_mode && manual_width != null && manual_height != null && manual_width > 0 && manual_height > 0) {
    applyManualCanvasStyle(manual_width, manual_height, scaleLocallyManual);
    disableAutoResize();
    console.log(`Initialized UI in Manual Resolution Mode: ${manual_width}x${manual_height} (logical), ScaleLocally: ${scaleLocallyManual}`);
  } else {
    const initialStreamWidth = 1024;
    const initialStreamHeight = 768;
    resetCanvasStyle(initialStreamWidth, initialStreamHeight);
    console.log("Initialized UI in Auto Resolution Mode (defaulting to 1024x768 logical for now)");
  }
  // desynchronized: low-latency hint for this main-thread present canvas (no
  // readback happens on it, so there's no downside).
  canvasContext = canvas.getContext('2d', { desynchronized: true });
  if (!canvasContext) {
    console.error('Failed to get 2D rendering context');
  }

  playButtonElement = document.createElement('button');
  playButtonElement.id = 'playButton';
  playButtonElement.textContent = 'Play Stream';
  videoContainer.appendChild(playButtonElement);
  playButtonElement.classList.add('hidden');
  statusDisplayElement.classList.remove('hidden');
  const sidebarDiv = document.createElement('div');
  sidebarDiv.id = 'dev-sidebar';
  const hiddenFileInput = document.createElement('input');
  hiddenFileInput.type = 'file';
  hiddenFileInput.id = 'globalFileInput';
  hiddenFileInput.multiple = true;
  hiddenFileInput.style.display = 'none';
  document.body.appendChild(hiddenFileInput);
  hiddenFileInput.addEventListener('change', handleFileInputChange);

  if (!document.getElementById('keyboard-input-assist')) {
    const keyboardInputAssist = document.createElement('input');
    keyboardInputAssist.type = 'search';
    keyboardInputAssist.id = 'keyboard-input-assist';
    keyboardInputAssist.style.position = 'absolute';
    keyboardInputAssist.style.left = '-9999px';
    keyboardInputAssist.style.top = '-9999px';
    keyboardInputAssist.style.width = '1px';
    keyboardInputAssist.style.height = '1px';
    keyboardInputAssist.style.opacity = '0';
    keyboardInputAssist.style.border = '0';
    keyboardInputAssist.style.padding = '0';
    keyboardInputAssist.style.caretColor = 'transparent';
    keyboardInputAssist.setAttribute('aria-hidden', 'true');
    keyboardInputAssist.setAttribute('autocomplete', 'off');
    keyboardInputAssist.setAttribute('autocorrect', 'off');
    keyboardInputAssist.setAttribute('autocapitalize', 'off');
    keyboardInputAssist.setAttribute('spellcheck', 'false');
    document.body.appendChild(keyboardInputAssist);
    console.log("Dynamically added #keyboard-input-assist element.");
  }
  appDiv.appendChild(videoContainer);
  updateStatusDisplay();
  playButtonElement.addEventListener('click', playStream);

  if (isSharedMode) {
      updateUIForSharedMode();
  }
};

function clearAllVncStripeDecoders() {
  console.log("Clearing all VNC stripe decoders.");
  for (const yPos in vncStripeDecoders) {
    if (vncStripeDecoders.hasOwnProperty(yPos)) {
      const decoderInfo = vncStripeDecoders[yPos];
      if (decoderInfo.decoder && decoderInfo.decoder.state !== "closed") {
        try {
          decoderInfo.decoder.close();
          console.log(`Closed VNC stripe decoder for Y=${yPos}`);
        } catch (e) {
          console.error(`Error closing VNC stripe decoder for Y=${yPos}:`, e);
        }
      }
    }
  }
  vncStripeDecoders = {};
  console.log("All VNC stripe decoders and metadata cleared.");
}

function processPendingChunksForStripe(stripe_y_start) {
  const decoderInfo = vncStripeDecoders[stripe_y_start];
  if (!decoderInfo || decoderInfo.decoder.state !== "configured" || !decoderInfo.pendingChunks) {
    return;
  }
  console.log(`Processing ${decoderInfo.pendingChunks.length} pending chunks for stripe Y=${stripe_y_start}`);
  while (decoderInfo.pendingChunks.length > 0) {
    const pending = decoderInfo.pendingChunks.shift();
    const chunk = new EncodedVideoChunk({
      type: pending.type,
      timestamp: pending.timestamp,
      data: pending.data
    });
    try {
      decoderInfo.decoder.decode(chunk);
    } catch (e) {
      console.error(`Error decoding pending chunk for stripe Y=${stripe_y_start}:`, e, chunk);
    }
  }
}

let decodedStripesQueue = [];
// Newest JPEG-stripe frame id drawn per startY, so out-of-order older stripes are skipped.
let lastDrawnJpegStripeFrameId = {};
// A stripe is "stale" only if it trails the last drawn id by at most this many frames
// (out-of-order decode completion is small). The frame id is a uint16, so a larger modular
// gap means a fresh stripe after that row sat static for a long time (or the id wrapped) --
// drawing it instead of dropping it avoids wedging a row for up to ~half the id space.
const JPEG_STRIPE_REORDER_WINDOW = 256;

function clearStartVideoWatchdog() {
  if (startVideoWatchdogTimer !== null) {
    clearTimeout(startVideoWatchdogTimer);
    startVideoWatchdogTimer = null;
  }
  startVideoWatchdogAttempts = 0;
}

function onStartVideoWatchdogTimeout() {
  startVideoWatchdogTimer = null;
  // Tab hidden again (the visibilitychange path owns state now) or shared mode: stand down.
  if (document.hidden || isSharedMode) { startVideoWatchdogAttempts = 0; return; }
  // Socket not open: the disconnect/reconnect logic elsewhere handles recovery.
  if (!websocket || websocket.readyState !== WebSocket.OPEN) { startVideoWatchdogAttempts = 0; return; }
  startVideoWatchdogAttempts++;
  if (startVideoWatchdogAttempts <= START_VIDEO_WATCHDOG_MAX_ATTEMPTS) {
    console.warn(`No video after START_VIDEO; resend attempt ${startVideoWatchdogAttempts}/${START_VIDEO_WATCHDOG_MAX_ATTEMPTS}.`);
    try { websocket.send('START_VIDEO'); } catch (_) {}
    startVideoWatchdogTimer = setTimeout(onStartVideoWatchdogTimeout, START_VIDEO_WATCHDOG_MS);
  } else {
    // Resends didn't take: force a reconnect (onclose triggers the reconnect path).
    console.warn('START_VIDEO watchdog exhausted; forcing websocket reconnect.');
    startVideoWatchdogAttempts = 0;
    try { websocket.close(); } catch (_) {}
  }
}

function armStartVideoWatchdog() {
  // Restart the attempt count for this visibility cycle.
  if (startVideoWatchdogTimer !== null) clearTimeout(startVideoWatchdogTimer);
  startVideoWatchdogAttempts = 0;
  startVideoWatchdogTimer = setTimeout(onStartVideoWatchdogTimeout, START_VIDEO_WATCHDOG_MS);
}

function handleDecodedVncStripeFrame(yPos, frame) {
  // Full-frame H.264 ('h264enc' = NVENC/x264, 'openh264enc' = OpenH264, decoded
  // by the single yPos=0 decoder): present the freshest frame the instant it decodes,
  // for the lowest glass-to-glass latency, instead of parking it in the queue for the
  // next rAF. h264enc-striped composites partial-height stripes on the 2D canvas and so
  // still drains through the rAF path below.
  if (!isSharedMode && (currentEncoderMode === 'h264enc' || currentEncoderMode === 'openh264enc') && yPos === 0) {
    if (document.hidden || (clientMode === 'websockets' && !isVideoPipelineActive)) {
      try { frame.close(); } catch (e) {}
      return;
    }
    // A newer full frame supersedes anything still queued; drop stale frames so only
    // the latest is shown (mirrors the rAF drop-older behavior).
    if (decodedStripesQueue.length > 0) {
      for (const stale of decodedStripesQueue) { try { stale.frame.close(); } catch (e) {} }
      decodedStripesQueue.length = 0;
    }
    if (supportsWindowMSTG && presentFrameToVideo(frame)) {
      // handed to the main-thread <video> track generator (zero-copy)
    } else if (USE_OFFSCREEN_WORKER && presentFrameToWorker(frame)) {
      // handed to the worker sink (VideoTrackGenerator <video>, or OffscreenCanvas)
    } else {
      if (canvas && canvasContext && canvas.width > 0 && canvas.height > 0) {
        canvasContext.drawImage(frame, 0, 0);
      }
      try { frame.close(); } catch (e) {}
    }
    if (!streamStarted) startStream();
    return;
  }
  decodedStripesQueue.push({
    yPos,
    frame
  });
}

async function handleAdvancedAudioClick() {
  console.log("Advanced Audio Settings button clicked.");
  if (!audioDeviceSettingsDivElement || !audioInputSelectElement || !audioOutputSelectElement) {
    console.error("Audio device UI elements not found in dev sidebar.");
    return;
  }
  const isHidden = audioDeviceSettingsDivElement.classList.contains('hidden');
  if (isHidden) {
    console.log("Settings are hidden, attempting to show and populate...");
    const supportsSinkId = typeof AudioContext !== 'undefined' && 'setSinkId' in AudioContext.prototype;
    const outputLabel = document.getElementById('audioOutputLabel');
    if (!supportsSinkId) {
      console.warn('Browser does not support selecting audio output device (setSinkId). Hiding output selection.');
      if (outputLabel) outputLabel.classList.add('hidden');
      audioOutputSelectElement.classList.add('hidden');
    } else {
      if (outputLabel) outputLabel.classList.remove('hidden');
      audioOutputSelectElement.classList.remove('hidden');
    }
    try {
      console.log("Requesting microphone permission for device listing...");
      const tempStream = await navigator.mediaDevices.getUserMedia({
        audio: true
      });
      tempStream.getTracks().forEach(track => track.stop());
      console.log("Microphone permission granted or already available (temporary stream stopped).");
      console.log("Enumerating media devices...");
      const devices = await navigator.mediaDevices.enumerateDevices();
      console.log("Devices found:", devices);
      audioInputSelectElement.innerHTML = '';
      audioOutputSelectElement.innerHTML = '';
      let inputCount = 0;
      let outputCount = 0;
      devices.forEach(device => {
        if (device.kind === 'audioinput') {
          inputCount++;
          const option = document.createElement('option');
          option.value = device.deviceId;
          option.textContent = device.label || `Microphone ${inputCount}`;
          audioInputSelectElement.appendChild(option);
        } else if (device.kind === 'audiooutput' && supportsSinkId) {
          outputCount++;
          const option = document.createElement('option');
          option.value = device.deviceId;
          option.textContent = device.label || `Speaker ${outputCount}`;
          audioOutputSelectElement.appendChild(option);
        }
      });
      console.log(`Populated ${inputCount} input devices and ${outputCount} output devices.`);
      audioDeviceSettingsDivElement.classList.remove('hidden');
    } catch (err) {
      console.error('Error getting media devices or permissions:', err);
      audioDeviceSettingsDivElement.classList.add('hidden');
      alert(`Could not list audio devices. Please ensure microphone permissions are granted.\nError: ${err.message || err.name}`);
    }
  } else {
    console.log("Settings are visible, hiding...");
    audioDeviceSettingsDivElement.classList.add('hidden');
  }
}

function handleAudioDeviceChange(event) {
  const selectedDeviceId = event.target.value;
  const isInput = event.target.id === 'audioInputSelect';
  const contextType = isInput ? 'input' : 'output';
  console.log(`Dev Sidebar: Audio device selected - Type: ${contextType}, ID: ${selectedDeviceId}. Posting message...`);
  window.postMessage({
    type: 'audioDeviceSelected',
    context: contextType,
    deviceId: selectedDeviceId
  }, window.location.origin);
}

function handleRequestFileUpload() {
  if (isSharedMode) {
    console.log("Shared mode: File upload via requestFileUpload blocked.");
    return;
  }
  const hiddenInput = document.getElementById('globalFileInput');
  if (!hiddenInput) {
    console.error("Global file input not found!");
    return;
  }
  if (!websocket || websocket.readyState !== WebSocket.OPEN) {
    console.warn("WebSocket is not open. File upload cannot be initiated.");
    return;
  }
  console.log("Triggering click on hidden file input.");
  hiddenInput.click();
}

async function handleFileInputChange(event) {
  if (isSharedMode) {
    console.log("Shared mode: File upload via fileInputChange blocked.");
    event.target.value = null;
    return;
  }
  const files = event.target.files;
  if (!files || files.length === 0) {
    event.target.value = null;
    return;
  }
  console.log(`File input changed, processing ${files.length} files sequentially.`);
  if (!websocket || websocket.readyState !== WebSocket.OPEN) {
    console.error("WebSocket is not open. Cannot upload selected files.");
    window.postMessage({
      type: 'fileUpload',
      payload: {
        status: 'error',
        fileName: 'N/A',
        message: "WebSocket not open for upload."
      }
    }, window.location.origin);
    event.target.value = null;
    return;
  }
  try {
    for (let i = 0; i < files.length; i++) {
      const file = files[i];
      const pathToSend = file.name;
      console.log(`Uploading file ${i + 1}/${files.length}: ${pathToSend}`);
      await uploadFileObject(file, pathToSend);
    }
    console.log("Finished processing all files from input.");
  } catch (error) {
    const errorMsg = `An error occurred during the file input upload process: ${error.message || error}`;
    console.error(errorMsg);
    window.postMessage({
      type: 'fileUpload',
      payload: {
        status: 'error',
        fileName: 'N/A',
        message: errorMsg
      }
    }, window.location.origin);
    if (websocket && websocket.readyState === WebSocket.OPEN) {
      try {
        websocket.send(`FILE_UPLOAD_ERROR:${b64Path('GENERAL')}:File input processing failed`);
      } catch (_) {}
    }
  } finally {
    event.target.value = null;
  }
}

/**
 * Requests a screen wake lock to prevent the device from sleeping.
 */
const requestWakeLock = async () => {
  if (wakeLockSentinel !== null) return;
  if ('wakeLock' in navigator) {
    try {
      wakeLockSentinel = await navigator.wakeLock.request('screen');
      wakeLockSentinel.addEventListener('release', () => {
        console.log('Screen Wake Lock was released automatically.');
        wakeLockSentinel = null;
      });
      console.log('Screen Wake Lock is active.');
    } catch (err) {
      console.error(`Could not acquire Wake Lock: ${err.name}, ${err.message}`);
    }
  } else {
    console.warn('Wake Lock API is not supported by this browser.');
  }
};

/**
 * Releases the screen wake lock if it is currently active.
 */
const releaseWakeLock = async () => {
  if (wakeLockSentinel !== null) {
    await wakeLockSentinel.release();
    wakeLockSentinel = null;
  }
};

function debounce(func, delay) {
  let timeoutId;
  return function(...args) {
    clearTimeout(timeoutId);
    timeoutId = setTimeout(() => {
      func.apply(this, args);
    }, delay);
  };
}

const startStream = () => {
  if (streamStarted) return;
  streamStarted = true;
  if (statusDisplayElement) statusDisplayElement.classList.add('hidden');
  if (playButtonElement) playButtonElement.classList.add('hidden');
  console.log("Stream started (UI elements hidden).");
};

const initializeInput = () => {
  if (inputInitialized) {
    console.log("Input already initialized. Skipping.");
    return;
  }
  if (clientSlot !== null && clientSlot > 0) {
    playerInputTargetIndex = clientSlot - 1;
    console.log(`Input Initialization: Applying server-provided slot ${clientSlot}. Gamepad will target index ${playerInputTargetIndex}.`);
  }
  inputInitialized = true;
  console.log("Initializing Input system...");

  let inputInstance;
  const websocketSendInput = (message) => {
    if (websocket && websocket.readyState === WebSocket.OPEN) {
      websocket.send(message);
    } else {
      console.warn("initializeInput: WebSocket not open, cannot send input message:", message);
    }
  };

  const sendInputFunction = websocketSendInput;

  if (!overlayInput) {
    console.error("initializeInput: overlayInput element not found. Cannot initialize input handling.");
    inputInitialized = false;
    return;
  }

  const initialSlot = clientSlot;
  inputInstance = new Input(overlayInput, sendInputFunction, isSharedMode, playerInputTargetIndex, useCssScaling, initialSlot);

  inputInstance.getWindowResolution = () => {
    const videoContainer = document.querySelector('.video-container');
    if (!videoContainer) {
      console.warn('initializeInput: .video-container not found, using window inner dimensions for resolution calculation.');
      return [window.innerWidth, window.innerHeight];
    }
    const videoContainerRect = videoContainer.getBoundingClientRect();
    return [videoContainerRect.width, videoContainerRect.height];
  };

  inputInstance.ongamepadconnected = (gamepad_id) => {
    gamepad.gamepadState = 'connected';
    gamepad.gamepadName = gamepad_id;
    console.log(`Client: Gamepad "${gamepad_id}" connected. isSharedMode: ${isSharedMode}, isGamepadEnabled (global toggle): ${isGamepadEnabled}`);
    if (window.webrtcInput && window.webrtcInput.gamepadManager) {
        if (isSharedMode) {
            window.webrtcInput.gamepadManager.enable();
            console.log("Shared mode: Gamepad connected, ensuring its GamepadManager is active for polling.");
        } else {
            if (!isGamepadEnabled) {
                window.webrtcInput.gamepadManager.disable();
                console.log("Primary mode: Gamepad connected, but master gamepad toggle is OFF. Disabling its GamepadManager.");
            } else {
                window.webrtcInput.gamepadManager.enable();
                console.log("Primary mode: Gamepad connected, master gamepad toggle is ON. Ensuring its GamepadManager is active.");
            }
        }
    } else {
        console.warn("Client: window.webrtcInput.gamepadManager not found in ongamepadconnected. Cannot control its polling state.");
    }
  };

  inputInstance.ongamepaddisconnected = () => {
    gamepad.gamepadState = 'disconnected';
    gamepad.gamepadName = 'none';
    console.log("Gamepad disconnected.");
  };

  inputInstance.attach();
  if (clientRole === 'viewer') {
      const reason = clientSlot !== null ? `(gamepad-only slot ${clientSlot})` : "(no slot)";
      console.log(`Role is 'viewer' ${reason}. Detaching context to disable mouse/keyboard/touch.`);
      inputInstance.detach_context();
  }
  window.webrtcInput = inputInstance;
  applyEffectiveCursorSetting();

  if (overlayInput) {
    const handlePointerDown = (e) => {
      requestWakeLock();
    };
    overlayInput.removeEventListener('pointerdown', handlePointerDown);
    overlayInput.addEventListener('pointerdown', handlePointerDown);
    overlayInput.addEventListener('contextmenu', e => {
      e.preventDefault();
    });
  }

  const handleResizeUI = () => {
    if (!initializationComplete) {
        return;
    }
    if (isSharedMode) {
        console.log("Shared mode: handleResizeUI (auto-resize logic) skipped.");
        if (manual_width && manual_height && manual_width > 0 && manual_height > 0) {
            applyManualCanvasStyle(manual_width, manual_height, true);
        }
        return;
    }
    if (window.is_manual_resolution_mode) {
      console.log("handleResizeUI: Auto-resize skipped, manual resolution mode is active.");
      return;
    }

    console.log("handleResizeUI: Auto-resize triggered (e.g., by window resize event).");
    const windowResolution = inputInstance.getWindowResolution();
    let evenWidth = alignResolution(windowResolution[0]);
    let evenHeight = alignResolution(windowResolution[1]);

    const dpr = useCssScaling ? 1 : (window.devicePixelRatio || 1);
    const MAX_DIM = 4080;
    
    if (evenWidth * dpr > MAX_DIM) {
        evenWidth = Math.floor(MAX_DIM / dpr);
        evenWidth = alignResolution(evenWidth);
    }
    if (evenHeight * dpr > MAX_DIM) {
        evenHeight = Math.floor(MAX_DIM / dpr);
        evenHeight = alignResolution(evenHeight);
    }

    if (evenWidth <= 0 || evenHeight <= 0) {
      console.warn(`handleResizeUI: Calculated invalid dimensions (${evenWidth}x${evenHeight}). Skipping resize send.`);
      return;
    }

    sendResolutionToServer(evenWidth, evenHeight);
    resetCanvasStyle(evenWidth, evenHeight);
  };

  handleResizeUI_globalRef = handleResizeUI;
  originalWindowResizeHandler = debounce(handleResizeUI, 500);

  if (isSharedMode) {
    console.log("Shared mode: Auto-resize event listener (originalWindowResizeHandler) NOT attached.");
  } else if (!window.is_manual_resolution_mode) {
    console.log("initializeInput: Auto-resolution mode. Attaching 'resize' event listener for subsequent changes.");
    window.addEventListener('resize', originalWindowResizeHandler);
    const videoContainer = document.querySelector('.video-container');
    let currentAutoWidth, currentAutoHeight;
    if (videoContainer) {
      const rect = videoContainer.getBoundingClientRect();
      currentAutoWidth = alignResolution(rect.width);
      currentAutoHeight = alignResolution(rect.height);
    } else {
      currentAutoWidth = alignResolution(window.innerWidth);
      currentAutoHeight = alignResolution(window.innerHeight);
    }
    if (currentAutoWidth <= 0 || currentAutoHeight <= 0) {
      console.warn(`initializeInput: Current auto-calculated dimensions are invalid (${currentAutoWidth}x${currentAutoHeight}). Defaulting canvas style to 1024x768 (logical) for initial setup. The resolution sent by onopen should prevail on the server.`);
      currentAutoWidth = 1024;
      currentAutoHeight = 768;
    }
    resetCanvasStyle(currentAutoWidth, currentAutoHeight);
    console.log(`initializeInput: Canvas style reset to reflect current auto-dimensions: ${currentAutoWidth}x${currentAutoHeight} (logical). Initial resolution was already sent by onopen.`);
  } else {
    console.log("initializeInput: Manual resolution mode active. Initial resolution already sent by onopen.");
    if (manual_width != null && manual_height != null && manual_width > 0 && manual_height > 0) {
      disableAutoResize();
    } else {
      console.warn("initializeInput: Manual mode is set, but manual_width/Height are invalid. Canvas might not display correctly.");
    }
  }

  if (overlayInput && !isSharedMode) {
    overlayInput.addEventListener('dragover', handleDragOver);
    overlayInput.addEventListener('drop', handleDrop);
  } else if (overlayInput && isSharedMode) {
    console.log("Shared mode: Drag/drop file upload listeners NOT attached to overlayInput.");
  } else {
    console.warn("initializeInput: overlayInput not found, cannot attach drag/drop listeners.");
  }

  const keyboardInputAssist = document.getElementById('keyboard-input-assist');
  if (keyboardInputAssist && inputInstance && !isSharedMode) {
    keyboardInputAssist.addEventListener('input', (event) => {
      const typedString = keyboardInputAssist.value;
      if (typedString) {
        inputInstance._typeString(typedString);
        keyboardInputAssist.value = '';
      }
    });
    keyboardInputAssist.addEventListener('keydown', (event) => {
      if (event.key === 'Enter' || event.keyCode === 13) {
        const enterKeysym = 0xFF0D;
        inputInstance._guac_press(enterKeysym);
        setTimeout(() => inputInstance._guac_release(enterKeysym), 5);
        event.preventDefault();
        keyboardInputAssist.value = '';
      } else if (event.key === 'Backspace' || event.keyCode === 8) {
        const backspaceKeysym = 0xFF08;
        inputInstance._guac_press(backspaceKeysym);
        setTimeout(() => inputInstance._guac_release(backspaceKeysym), 5);
        event.preventDefault();
      }
    });
    console.log("initializeInput: Added 'input' and 'keydown' listeners to #keyboard-input-assist.");
  } else if (isSharedMode) {
    console.log("Shared mode: Keyboard input assist listeners NOT attached.");
  } else {
    console.error("initializeInput: Could not add listeners to keyboard assist: Element or Input handler instance not found.");
  }
  console.log("Input system initialized.");
};

async function applyOutputDevice() {
  if (!preferredOutputDeviceId) {
    console.log("No preferred output device set, using default.");
    return;
  }
  const supportsSinkId = (typeof AudioContext !== 'undefined' && 'setSinkId' in AudioContext.prototype) ||
    (audioElement && typeof audioElement.setSinkId === 'function');
  if (!supportsSinkId) {
    console.warn("Browser does not support setSinkId, cannot apply output device preference.");
    if (audioOutputSelectElement) audioOutputSelectElement.classList.add('hidden');
    const outputLabel = document.getElementById('audioOutputLabel');
    if (outputLabel) outputLabel.classList.add('hidden');
    return;
  }
  if (audioContext) {
    if (audioContext.state === 'running') {
      try {
        await audioContext.setSinkId(preferredOutputDeviceId);
        console.log(`Playback AudioContext output set to device: ${preferredOutputDeviceId}`);
      } catch (err) {
        console.error(`Error setting sinkId on Playback AudioContext (ID: ${preferredOutputDeviceId}): ${err.name}`, err);
      }
    } else {
      console.warn(`Playback AudioContext not running (state: ${audioContext.state}), cannot set sinkId yet.`);
    }
  } else {
    console.log("Playback AudioContext doesn't exist yet, sinkId will be applied on initialization.");
  }
}

window.addEventListener('message', receiveMessage, false);

function postSidebarButtonUpdate() {
  const updatePayload = {
    type: 'sidebarButtonStatusUpdate',
    video: isVideoPipelineActive,
    audio: isAudioPipelineActive,
    microphone: isMicrophoneActive,
    gamepad: isGamepadEnabled
  };
  console.log('Posting sidebarButtonStatusUpdate:', updatePayload);
  window.postMessage(updatePayload, window.location.origin);
}

function receiveMessage(event) {
  if (event.origin !== window.location.origin) {
    console.warn(`Received message from unexpected origin: ${event.origin}. Expected ${window.location.origin}. Ignoring.`);
    return;
  }
  const message = event.data;
  if (typeof message !== 'object' || message === null) {
    console.warn('Received non-object message via window.postMessage:', message);
    return;
  }
  if (!message.type) {
    console.warn('Received message without a type property:', message);
    return;
  }
  switch (message.type) {
    case 'setVolume':
      if (typeof message.value === 'number' && audioGainNode) {
        currentVolume = Math.max(0, Math.min(1, message.value));
        audioGainNode.gain.setValueAtTime(currentVolume, audioContext.currentTime);
      }
      break;
    case 'setMute':
      if (typeof message.value === 'boolean' && audioGainNode) {
        if (message.value === true) {
          audioGainNode.gain.setValueAtTime(0, audioContext.currentTime);
        } else {
          audioGainNode.gain.setValueAtTime(currentVolume, audioContext.currentTime);
        }
      }
      break;
    case 'sidebarVisibilityChanged':
      isSidebarOpen = !!message.isOpen;
      break;
    case 'setScaleLocally':
      if (isSharedMode) {
        console.log("Shared mode: setScaleLocally message ignored (forced true behavior).");
        break;
      }
      if (typeof message.value === 'boolean') {
        scaleLocallyManual = message.value;
        setBoolParam('scaleLocallyManual', scaleLocallyManual);
        console.log(`Set scaleLocallyManual to ${scaleLocallyManual} and persisted.`);
        if (window.is_manual_resolution_mode && manual_width !== null && manual_height !== null) {
          console.log("Applying new scaling style in manual mode.");
          applyManualCanvasStyle(manual_width, manual_height, scaleLocallyManual);
        }
      } else {
        console.warn("Invalid value received for setScaleLocally:", message.value);
      }
      break;
    case 'setSynth':
      if (window.webrtcInput && typeof window.webrtcInput.setSynth === 'function') {
        window.webrtcInput.setSynth(message.value);
      }
      break;
    case 'showVirtualKeyboard':
      if (isSharedMode) {
        console.log("Shared mode: showVirtualKeyboard message ignored.");
        break;
      }
      console.log("Received 'showVirtualKeyboard' message.");
      const kbdAssistInput = document.getElementById('keyboard-input-assist');
      const mainInteractionOverlay = document.getElementById('overlayInput');
      if (kbdAssistInput) {
        kbdAssistInput.value = '';
        kbdAssistInput.focus();
        console.log("Focused #keyboard-input-assist element.");
        mainInteractionOverlay.addEventListener(
          "touchstart",
          () => {
            if (document.activeElement === kbdAssistInput) {
              kbdAssistInput.blur();
            }
          }, {
            once: true,
            passive: true
          }
        );
      } else {
        console.error("Could not find #keyboard-input-assist element to focus.");
      }
      break;
    case 'setUseCssScaling':
      if (typeof message.value === 'boolean') {
        const changed = useCssScaling !== message.value;
        useCssScaling = message.value;
        setBoolParam('useCssScaling', useCssScaling);
        console.log(`Set useCssScaling to ${useCssScaling} and persisted.`);

        if (window.webrtcInput && typeof window.webrtcInput.updateCssScaling === 'function') {
          window.webrtcInput.updateCssScaling(useCssScaling);
        }
        if (changed) {
          updateCanvasImageRendering();
          if (window.is_manual_resolution_mode && manual_width != null && manual_height != null) {
            sendResolutionToServer(manual_width, manual_height);
            applyManualCanvasStyle(manual_width, manual_height, scaleLocallyManual);
          } else if (!isSharedMode) {
            const currentWindowRes = window.webrtcInput ? window.webrtcInput.getWindowResolution() : [window.innerWidth, window.innerHeight];
            const autoWidth = alignResolution(currentWindowRes[0]);
            const autoHeight = alignResolution(currentWindowRes[1]);
            sendResolutionToServer(autoWidth, autoHeight);
            resetCanvasStyle(autoWidth, autoHeight);
          } else {
             if (manual_width && manual_height) {
                applyManualCanvasStyle(manual_width, manual_height, true);
             }
          }
          if (currentEncoderMode !== 'jpeg' && currentEncoderMode !== 'h264enc' && currentEncoderMode !== 'openh264enc' && currentEncoderMode !== 'h264enc-striped') {
            triggerInitializeDecoder();
          }
        }
      } else {
        console.warn("Invalid value received for setUseCssScaling:", message.value);
      }
      break;
    case 'setAntiAliasing':
      if (typeof message.value === 'boolean') {
        const changed = antiAliasingEnabled !== message.value;
        antiAliasingEnabled = message.value;
        setBoolParam('antiAliasingEnabled', antiAliasingEnabled);
        console.log(`Set antiAliasingEnabled to ${antiAliasingEnabled} and persisted.`);
        if (changed) {
          updateCanvasImageRendering();
        }
      } else {
        console.warn("Invalid value received for setAntiAliasing:", message.value);
      }
      break;
    case 'setUseBrowserCursors':
      if (typeof message.value === 'boolean') {
        use_browser_cursors = message.value;
        setBoolParam('use_browser_cursors', use_browser_cursors);
        console.log(`Set use_browser_cursors to ${use_browser_cursors} and persisted.`);
        applyEffectiveCursorSetting();
      } else {
        console.warn("Invalid value received for setUseBrowserCursors:", message.value);
      }
      break;
    case 'setManualResolution':
      if (isSharedMode) {
        console.log("Shared mode: setManualResolution message ignored.");
        break;
      }
      const width = parseInt(message.width, 10);
      const height = parseInt(message.height, 10);
      if (isNaN(width) || width <= 0 || isNaN(height) || height <= 0) {
        console.error('Received invalid width/height for setManualResolution:', message);
        break;
      }
      console.log(`Setting manual resolution: ${width}x${height} (logical)`);
      window.is_manual_resolution_mode = true;
      manual_width = alignResolution(width);
      manual_height = alignResolution(height);
      console.log(`Rounded logical resolution to even numbers: ${manual_width}x${manual_height}`);
      setIntParam('manual_width', manual_width);
      setIntParam('manual_height', manual_height);
      setBoolParam('is_manual_resolution_mode', true);
      disableAutoResize();
      sendResolutionToServer(manual_width, manual_height);
      applyManualCanvasStyle(manual_width, manual_height, scaleLocallyManual);
      if (currentEncoderMode === 'h264enc' || currentEncoderMode === 'openh264enc' || currentEncoderMode === 'h264enc-striped') {
        console.log("Clearing VNC stripe decoders due to manual resolution change.");
        clearAllVncStripeDecoders();
        if (canvasContext) canvasContext.setTransform(1, 0, 0, 1, 0, 0);
        canvasContext.clearRect(0, 0, canvas.width, canvas.height);
      }
      break;
    case 'resetResolutionToWindow':
      if (isSharedMode) {
        console.log("Shared mode: resetResolutionToWindow message ignored.");
        break;
      }
      console.log("Resetting resolution to window size.");
      window.is_manual_resolution_mode = false;
      manual_width = null;
      manual_height = null;
      setIntParam('manual_width', null);
      setIntParam('manual_height', null);
      setBoolParam('is_manual_resolution_mode', false);
      const currentWindowRes = window.webrtcInput ? window.webrtcInput.getWindowResolution() : [window.innerWidth, window.innerHeight];
      const autoWidth = alignResolution(currentWindowRes[0]);
      const autoHeight = alignResolution(currentWindowRes[1]);
      resetCanvasStyle(autoWidth, autoHeight);
      if (currentEncoderMode === 'h264enc' || currentEncoderMode === 'openh264enc' || currentEncoderMode === 'h264enc-striped') {
        console.log("Clearing VNC stripe decoders due to resolution reset to window.");
        clearAllVncStripeDecoders();
        if (canvasContext) canvasContext.setTransform(1, 0, 0, 1, 0, 0);
        canvasContext.clearRect(0, 0, canvas.width, canvas.height);
      }
      enableAutoResize();
      break;
    case 'settings':
      console.log('Received settings message:', message.settings);
      handleSettingsMessage(message.settings);
      break;
    case 'getStats':
      console.log('Received getStats message.');
      sendStatsMessage();
      break;
    case 'clipboardUpdateFromUI':
      console.log('Received clipboardUpdateFromUI message.');
      if (isSharedMode) {
        console.log("Shared mode: Clipboard write to server blocked.");
        break;
      }
      const newClipboardText = message.text;
      sendClipboardData(newClipboardText);
      break;
    case 'pipelineStatusUpdate':
      console.log('Received pipelineStatusUpdate message:', message);
      let stateChangedFromStatus = false;
      if (message.video !== undefined && isVideoPipelineActive !== message.video) {
        isVideoPipelineActive = message.video;
        stateChangedFromStatus = true;
      }
      if (message.audio !== undefined && isAudioPipelineActive !== message.audio) {
        isAudioPipelineActive = message.audio;
        stateChangedFromStatus = true;
      }
      if (message.microphone !== undefined && isMicrophoneActive !== message.microphone) {
        isMicrophoneActive = message.microphone;
        stateChangedFromStatus = true;
      }
      if (message.gamepad !== undefined && isGamepadEnabled !== message.gamepad) {
        isGamepadEnabled = message.gamepad;
        stateChangedFromStatus = true;
      }
      if (stateChangedFromStatus) {
        postSidebarButtonUpdate();
      }
      break;
    case 'pipelineControl':
      console.log(`Received pipeline control message: pipeline=${message.pipeline}, enabled=${message.enabled}`);
      const pipeline = message.pipeline;
      const desiredState = message.enabled;
      let stateChangedFromControl = false;
      let wsMessage = '';

      if (pipeline === 'video') {
        if (isSharedMode) {
          console.log("Shared mode: Video pipelineControl blocked.");
          break;
        }
        if (isVideoPipelineActive !== desiredState) {
          isVideoPipelineActive = desiredState;
          stateChangedFromControl = true;
          wsMessage = desiredState ? 'START_VIDEO' : 'STOP_VIDEO';

          if (!desiredState) {
            console.log("Client: STOP_VIDEO requested via pipelineControl. Clearing canvas visually. Server will send PIPELINE_RESETTING for full state reset.");
            if (canvasContext && canvas) {
              try {
                canvasContext.setTransform(1, 0, 0, 1, 0, 0);
                canvasContext.clearRect(0, 0, canvas.width, canvas.height);
              } catch (e) { console.error("Error clearing canvas on STOP_VIDEO request:", e); }
            }
          } else {
            console.log("Client: START_VIDEO requested via pipelineControl. Clearing canvas visually. Server will send PIPELINE_RESETTING for full state reset.");
             if (canvasContext && canvas) {
                try {
                    canvasContext.setTransform(1, 0, 0, 1, 0, 0);
                    canvasContext.clearRect(0, 0, canvas.width, canvas.height);
                } catch (e) { console.error("Error clearing canvas on START_VIDEO request:", e); }
            }
          }
        }
      } else if (pipeline === 'audio') {
        if (displayId !== 'primary') {
            console.log("Secondary display: Audio control blocked.");
            break;
        }
        if (!audioEnabled) {
          console.log("Audio is disabled. Audio pipeline control blocked.");
          break;
        }
        if (isAudioPipelineActive !== desiredState) {
          isAudioPipelineActive = desiredState;
          stateChangedFromControl = true;
          wsMessage = desiredState ? 'START_AUDIO' : 'STOP_AUDIO';
          if (audioDecoderWorker) {
            audioDecoderWorker.postMessage({
              type: 'updatePipelineStatus',
              data: {
                isActive: isAudioPipelineActive
              }
            });
          }
        }
      } else if (pipeline === 'microphone') {
        if (isSharedMode) {
          console.log("Shared mode: Microphone control blocked.");
          break;
        }
        if (!microphoneEnabled) {
          console.log("Microphone is disabled. Microphone pipeline control blocked.");
          break;
        }
        if (desiredState) {
          startMicrophoneCapture();
        } else {
          stopMicrophoneCapture();
        }
      } else {
        console.warn(`Received pipelineControl message for unknown pipeline: ${pipeline}`);
      }

      if (wsMessage && websocket && websocket.readyState === WebSocket.OPEN) {
        try {
          websocket.send(wsMessage);
          console.log(`Sent command to server via WebSocket: ${wsMessage}`);
        } catch (e) {
          console.error(`Error sending ${wsMessage} to WebSocket:`, e);
        }
      }
      break;
    case 'audioDeviceSelected':
      console.log('Received audioDeviceSelected message:', message);
      if (isSharedMode && message.context === 'input') {
          console.log("Shared mode: Audio input device selection ignored.");
          break;
      }
      if (!audioEnabled) {
          console.log("Audio control flag is disabled. Audio device selection blocked.");
          break;
      }
      const {
        context, deviceId
      } = message;
      if (!deviceId) {
        console.warn("Received audioDeviceSelected message without a deviceId.");
        break;
      }
      if (context === 'input') {
        preferredInputDeviceId = deviceId;
        if (isMicrophoneActive) {
          stopMicrophoneCapture();
          setTimeout(startMicrophoneCapture, 150);
        }
      } else if (context === 'output') {
        preferredOutputDeviceId = deviceId;
        applyOutputDevice();
      } else {
        console.warn(`Unknown context in audioDeviceSelected message: ${context}`);
      }
      break;
    case 'gamepadControl':
      console.log(`Received gamepad control message: enabled=${message.enabled}`);
      const newGamepadState = message.enabled;
      if (isGamepadEnabled !== newGamepadState) {
        isGamepadEnabled = newGamepadState;
        setBoolParam('isGamepadEnabled', isGamepadEnabled);
        postSidebarButtonUpdate();
        if (window.webrtcInput && window.webrtcInput.gamepadManager) {
            if (isSharedMode) {
                window.webrtcInput.gamepadManager.enable();
                console.log("Shared mode: Gamepad control message received, ensuring its GamepadManager remains active for polling.");
            } else {
                if (isGamepadEnabled) {
                    window.webrtcInput.gamepadManager.enable();
                    console.log("Primary mode: Gamepad toggle ON. Enabling GamepadManager polling.");
                } else {
                    window.webrtcInput.gamepadManager.disable();
                    console.log("Primary mode: Gamepad toggle OFF. Disabling GamepadManager polling.");
                }
            }
        } else {
            console.warn("Client: window.webrtcInput.gamepadManager not found in 'gamepadControl' message handler.");
        }
      }
      break;
    case 'requestFullscreen':
      enterFullscreen();
      break;
    case 'command':
      if (isSharedMode) {
        console.log("Shared mode: Arbitrary command sending to server blocked.");
        break;
      }
      if (!serverCommandEnabled) {
        console.log("Command sending suppressed: server has command_enabled=false; not sending 'cmd,'.");
        break;
      }
      if (typeof message.value === 'string') {
        const commandString = message.value;
        console.log(`Received 'command' message with value: "${commandString}". Forwarding to WebSocket.`);
        if (websocket && websocket.readyState === WebSocket.OPEN) {
          try {
            websocket.send(`cmd,${commandString}`);
            console.log(`Sent command to server via WebSocket: cmd,${commandString}`);
          } catch (e) {
            console.error('Failed to send command via WebSocket:', e);
          }
        } else {
          console.warn('Cannot send command: WebSocket is not open or not available.');
        }
      } else {
        console.warn("Received 'command' message without a string value:", message);
      }
      break;
    case 'touchinput:trackpad':
      if (window.webrtcInput && typeof window.webrtcInput.setTrackpadMode === 'function') {
        trackpadMode = true;
        setBoolParam('trackpadMode', true);
        window.webrtcInput.setTrackpadMode(true);
        if (websocket && websocket.readyState === WebSocket.OPEN) {
          websocket.send("SET_NATIVE_CURSOR_RENDERING,1");
        }
      }
      break;
    case 'touchinput:touch':
      if (window.webrtcInput && typeof window.webrtcInput.setTrackpadMode === 'function') {
        trackpadMode = false;
        setBoolParam('trackpadMode', false);
        window.webrtcInput.setTrackpadMode(false);
        if (websocket && websocket.readyState === WebSocket.OPEN) {
          websocket.send("SET_NATIVE_CURSOR_RENDERING,0");
        }
      }
      break;
    default:
      break;
  }
}

// Settle pending REQUEST_CLIPBOARD promises with fresh server data: text -> string,
// binary -> Blob (text Blob if none). Called from the clipboard message handlers.
// NOTE: the wire protocol carries no request id, so any incoming server-clipboard
// message settles the oldest pending Ctrl/Cmd+C request; an unrelated server push
// arriving between request and response could settle it with the wrong content. The
// ~2s timeout + cache bound the impact; true correlation would need a server echo.
function resolveServerClipboard(text, blob, mime) {
    if (typeof text === 'string') { lastServerClipboardText = text; }
    if (blob) { lastServerClipboardBlob = blob; }
    if (mime) { lastServerClipboardMime = mime; }
    if (pendingClipboardRequests.length === 0) return;
    const reqs = pendingClipboardRequests;
    pendingClipboardRequests = [];
    for (const req of reqs) {
        if (req.settled) continue;
        try {
            // One-behind guard: the server reads its X clipboard the instant
            // REQUEST_CLIPBOARD arrives, which on a Ctrl/Cmd+C races AHEAD of the
            // app writing the new selection, so the first reply is the PRE-copy
            // value. Hold the request open until an incoming value DIFFERS from
            // what was cached when the request was made, then settle with the
            // fresh value. The bounded timeout is the floor if nothing ever differs.
            if (req.wantBinary) {
                if (blob && blob !== req.baselineBlob) {
                    req.resolve(blob);
                } else {
                    pendingClipboardRequests.push(req);
                }
            } else {
                if (typeof text === 'string' && text !== req.baselineText) {
                    req.resolve(text);
                } else {
                    pendingClipboardRequests.push(req);
                }
            }
        } catch (_) { /* ignore */ }
    }
}

// Ask the server for its current clipboard and return a Promise that resolves when
// the next server clipboard message arrives. Falls back to the cached value after
// ~1s so a non-responsive server can't hang the ClipboardItem promise (and the
// browser's transient-activation window) indefinitely.
function requestServerClipboard(wantBinary) {
    if (websocket && websocket.readyState === WebSocket.OPEN) {
        try { websocket.send('REQUEST_CLIPBOARD'); } catch (_) {}
    }
    return new Promise((resolve) => {
        // Snapshot the currently-cached server clipboard: replies matching it are
        // treated as the stale pre-copy read and skipped until a fresh value lands.
        const req = { wantBinary: !!wantBinary, resolve, settled: false,
            baselineText: lastServerClipboardText, baselineBlob: lastServerClipboardBlob };
        const done = (val) => {
            if (req.settled) return;
            req.settled = true;
            const idx = pendingClipboardRequests.indexOf(req);
            if (idx !== -1) pendingClipboardRequests.splice(idx, 1);
            resolve(val);
        };
        req.resolve = done;
        pendingClipboardRequests.push(req);
        // 2s (was 1s): give a slow server time to answer the first Ctrl/Cmd+C of a
        // session, when the cache is still empty, before falling back to it.
        setTimeout(() => {
            if (wantBinary) {
                done(lastServerClipboardBlob || new Blob([lastServerClipboardText || ''], { type: 'text/plain' }));
            } else {
                done(lastServerClipboardText || '');
            }
        }, 2000);
    });
}

async function sendClipboardData(data, mimeType = 'text/plain') {
    if (!window.clipboard_enabled || !clipboard_in_enabled) return;
    if (!websocket || websocket.readyState !== WebSocket.OPEN) {
        console.warn('Cannot send clipboard data: WebSocket is not open.');
        return;
    }
    const isBinary = data instanceof ArrayBuffer || data instanceof Uint8Array;
    let dataBytes;
    if (isBinary) {
        dataBytes = new Uint8Array(data);
    } else { 
        dataBytes = new TextEncoder().encode(data);
        mimeType = 'text/plain';
    }
    if (dataBytes.byteLength < CLIPBOARD_CHUNK_SIZE) {
        let binaryString = '';
        for (let i = 0; i < dataBytes.length; i++) {
            binaryString += String.fromCharCode(dataBytes[i]);
        }
        const base64Data = btoa(binaryString);
        if (mimeType === 'text/plain') {
            websocket.send(`cw,${base64Data}`);
            console.log('Sent small clipboard text in single message.');
        } else {
            websocket.send(`cb,${mimeType},${base64Data}`);
            console.log(`Sent small binary clipboard data in single message: ${mimeType}`);
        }
    } else {
        console.log(`Sending large clipboard data (${dataBytes.byteLength} bytes) in multiple parts.`);
        const totalSize = dataBytes.byteLength;
        const tid = ++clipboardTransferCounter;
        if (mimeType === 'text/plain') {
            websocket.send(`cws,${tid},${totalSize}`);
        } else {
            websocket.send(`cbs,${tid},${mimeType},${totalSize}`);
        }
        for (let offset = 0; offset < totalSize; offset += CLIPBOARD_CHUNK_SIZE) {
            const chunk = dataBytes.subarray(offset, offset + CLIPBOARD_CHUNK_SIZE);
            let binaryString = '';
            for (let i = 0; i < chunk.length; i++) {
                binaryString += String.fromCharCode(chunk[i]);
            }
            const base64Chunk = btoa(binaryString);

            if (mimeType === 'text/plain') {
                websocket.send(`cwd,${tid},${base64Chunk}`);
            } else {
                websocket.send(`cbd,${tid},${base64Chunk}`);
            }
            await new Promise(resolve => setTimeout(resolve, 0));
        }

        if (mimeType === 'text/plain') {
            websocket.send(`cwe,${tid}`);
        } else {
            websocket.send(`cbe,${tid}`);
        }
        console.log('Finished sending multi-part clipboard data.');
    }
}

function handleSettingsMessage(settings) {
  console.log('Applying settings:', settings);
  let settingsChanged = false;
  if (settings.framerate !== undefined) {
    framerate = parseInt(settings.framerate);
    setIntParam('framerate', framerate);
    settingsChanged = true;
  }
  if (settings.encoder !== undefined) {
    const newEncoderSetting = settings.encoder;
    if (currentEncoderMode !== newEncoderSetting) {
        currentEncoderMode = newEncoderSetting;
        setStringParam('encoder', currentEncoderMode);
        settingsChanged = true;
        if (newEncoderSetting === 'jpeg' || newEncoderSetting === 'h264enc' || newEncoderSetting === 'openh264enc' || newEncoderSetting === 'h264enc-striped') {
            if (decoder && decoder.state !== 'closed') {
                console.log(`Switching to ${newEncoderSetting}, closing main video decoder.`);
                decoder.close();
                decoder = null;
            }
        }
        if (newEncoderSetting !== 'h264enc-striped') {
            clearAllVncStripeDecoders();
        }
        // Flush render queues so the previous mode's frames are closed, not painted later.
        cleanupVideoBuffer();
        cleanupJpegStripeQueue();
        clearDecodedStripesQueue();
    }
  }
  if (settings.video_crf !== undefined) {
    video_crf = parseInt(settings.video_crf, 10);
    setIntParam('video_crf', video_crf);
    settingsChanged = true;
  }
  if (settings.video_fullcolor !== undefined) {
    video_fullcolor = !!settings.video_fullcolor;
    setBoolParam('video_fullcolor', video_fullcolor);
    settingsChanged = true;
    if (decoder && decoder.state !== 'closed') {
      console.log('video_fullcolor setting changed, closing main video decoder.');
      decoder.close();
      decoder = null;
    }
    clearAllVncStripeDecoders();
  }
  if (settings.video_streaming_mode !== undefined) {
    video_streaming_mode = !!settings.video_streaming_mode;
    setBoolParam('video_streaming_mode', video_streaming_mode);
    settingsChanged = true;
  }
  if (settings.jpeg_quality !== undefined) {
    jpeg_quality = parseInt(settings.jpeg_quality, 10);
    setIntParam('jpeg_quality', jpeg_quality);
    settingsChanged = true;
  }
  if (settings.paint_over_jpeg_quality !== undefined) {
    paint_over_jpeg_quality = parseInt(settings.paint_over_jpeg_quality, 10);
    setIntParam('paint_over_jpeg_quality', paint_over_jpeg_quality);
    settingsChanged = true;
  }
  if (settings.use_cpu !== undefined) {
    use_cpu = !!settings.use_cpu;
    setBoolParam('use_cpu', use_cpu);
    settingsChanged = true;
    if (decoder && decoder.state !== 'closed') {
      console.log('use_cpu setting changed, closing main video decoder.');
      decoder.close();
      decoder = null;
    }
    clearAllVncStripeDecoders();
  }
  if (settings.video_paintover_crf !== undefined) {
    video_paintover_crf = parseInt(settings.video_paintover_crf, 10);
    setIntParam('video_paintover_crf', video_paintover_crf);
    settingsChanged = true;
  }
  if (settings.video_paintover_burst_frames !== undefined) {
    video_paintover_burst_frames = parseInt(settings.video_paintover_burst_frames, 10);
    setIntParam('video_paintover_burst_frames', video_paintover_burst_frames);
    settingsChanged = true;
  }
  if (settings.use_paint_over_quality !== undefined) {
    use_paint_over_quality = !!settings.use_paint_over_quality;
    setBoolParam('use_paint_over_quality', use_paint_over_quality);
    settingsChanged = true;
  }
  if (settings.is_manual_resolution_mode === true) {
    scalingDPI = 96;
    setIntParam('scaling_dpi', scalingDPI);
    settingsChanged = true;
  }
  if (settings.scaling_dpi !== undefined) {
    scalingDPI = parseInt(settings.scaling_dpi, 10);
    setIntParam('scaling_dpi', scalingDPI);
    settingsChanged = true;
    if (websocket && websocket.readyState === WebSocket.OPEN) {
        console.log(`[websockets] Sending explicit DPI command: s,${scalingDPI}`);
        websocket.send(`s,${scalingDPI}`);
    }
  }
  if (settings.enable_binary_clipboard !== undefined) {
    enable_binary_clipboard = !!settings.enable_binary_clipboard;
    setBoolParam('enable_binary_clipboard', enable_binary_clipboard);
    settingsChanged = true;
  }
  if (settings.clipboard_in_enabled !== undefined) {
    clipboard_in_enabled = !!settings.clipboard_in_enabled;
    setBoolParam('clipboard_in_enabled', clipboard_in_enabled);
    settingsChanged = true;
  }
  if (settings.clipboard_out_enabled !== undefined) {
    clipboard_out_enabled = !!settings.clipboard_out_enabled;
    setBoolParam('clipboard_out_enabled', clipboard_out_enabled);
    settingsChanged = true;
  }
  if (settings.use_css_scaling !== undefined) {
    const messageData = { type: 'setUseCssScaling', value: !!settings.use_css_scaling };
    receiveMessage({ origin: window.location.origin, data: messageData });
  }
  if (settings.use_browser_cursors !== undefined) {
    use_browser_cursors = !!settings.use_browser_cursors;
    setBoolParam('use_browser_cursors', use_browser_cursors);
    applyEffectiveCursorSetting();
  }
  if (settings.debug !== undefined) {
    debug = settings.debug;
    setBoolParam('debug', debug);
    console.log(`Applied debug setting: ${debug}. Reloading...`);
    setTimeout(() => { window.location.reload(); }, 700);
    return;
  }
  if (settings.rate_control_mode !== undefined) {
    rateControlMode = settings.rate_control_mode;
    setStringParam('rate_control_mode', rateControlMode);
    fetchLatestRCvalue(rateControlMode);
    settingsChanged = true;
  }
  if (settings.video_bitrate !== undefined) {
    videoBitrate = parseFloat(settings.video_bitrate);
    setIntParam('video_bitrate', videoBitrate);
    settingsChanged = true;
  }
  if (settings.force_aligned_resolution !== undefined) {
    force_aligned_resolution = !!settings.force_aligned_resolution;
    setBoolParam('force_aligned_resolution', force_aligned_resolution);
    settingsChanged = true;
  }
  if (settingsChanged) {
    sendFullSettingsUpdateToServer('handleSettingsMessage');
  }
}

function fetchLatestRCvalue(newMode) {
  if (newMode === "cbr") {
    videoBitrate = getFloatParam('video_bitrate', videoBitrate);
  } else if (newMode === "crf") {
    video_crf = getIntParam('video_crf', video_crf);
  }
};

function sendStatsMessage() {
  const stats = {
    gpu: gpuStat,
    cpu: cpuStat,
    network: networkStat,
    clientFps: window.fps,
    audioBuffer: window.currentAudioBufferSize,
    audioUnderrunSamples: window.currentAudioUnderrunSamples,
    audioDropped: window.currentAudioDropped + window.currentAudioWorkletDropped,
    videoBuffer: videoFrameBuffer.length,
    isVideoPipelineActive: isVideoPipelineActive,
    isAudioPipelineActive: isAudioPipelineActive,
    isMicrophoneActive: isMicrophoneActive,
  };
  stats.encoderName = currentEncoderMode;
  stats.video_fullcolor = video_fullcolor;
  stats.video_streaming_mode = video_streaming_mode;
  window.parent.postMessage({
    type: 'stats',
    data: stats
  }, window.location.origin);
  console.log('Sent stats message via window.postMessage:', stats);
}

function initWebsockets() {
  async function initializeDecoder() {
    mainDecoderHasKeyframe = false;
    if (decoder && decoder.state !== 'closed') {
      console.warn("VideoDecoder already exists, closing before re-initializing.");
      decoder.close();
    }
    let targetWidth = 1024;
    let targetHeight = 768;
    if (isSharedMode) {
        targetWidth = manual_width > 0 ? manual_width : 1024;
        targetHeight = manual_height > 0 ? manual_height : 768;
    } else if (window.is_manual_resolution_mode && manual_width != null && manual_height != null) {
      targetWidth = manual_width;
      targetHeight = manual_height;
    } else if (window.webrtcInput && typeof window.webrtcInput.getWindowResolution === 'function') {
      try {
        const currentRes = window.webrtcInput.getWindowResolution();
        const autoWidth = alignResolution(currentRes[0]);
        const autoHeight = alignResolution(currentRes[1]);
        if (autoWidth > 0 && autoHeight > 0) {
          targetWidth = autoWidth;
          targetHeight = autoHeight;
        }
      } catch (e) { /* use defaults */ }
    }

    const dpr = useCssScaling ? 1 : (window.devicePixelRatio || 1);
    const actualCodedWidth = alignResolution(targetWidth * dpr);
    const actualCodedHeight = alignResolution(targetHeight * dpr);

    decoder = new VideoDecoder({
      output: handleDecodedFrame,
      error: (e) => initiateFallback(e, 'main_decoder'),
    });
    const dynamicCodec = getDynamicH264Codec(actualCodedWidth, actualCodedHeight, video_fullcolor, framerate);
    const decoderConfig = {
      codec: dynamicCodec,
      codedWidth: actualCodedWidth,
      codedHeight: actualCodedHeight,
      optimizeForLatency: true
    };
    try {
      const support = await VideoDecoder.isConfigSupported(decoderConfig);
      if (!support.supported) {
        throw new Error(`Configuration not supported: ${JSON.stringify(decoderConfig)}`);
      }
      await decoder.configure(decoderConfig);
      configuredMainCodec = dynamicCodec;
      mainDecoderCodedWidth = actualCodedWidth;
      mainDecoderCodedHeight = actualCodedHeight;
      console.log('Main VideoDecoder configured successfully with config:', decoderConfig);
      if (isSharedMode && pendingSharedKeyframe) {
        console.log('Shared mode: Decoding keyframe stashed while the decoder was initializing.');
        // Adopt the stashed keyframe's in-band SPS (Chromium only) before decoding.
        maybeReconfigureMainDecoderFromSps(new Uint8Array(pendingSharedKeyframe));
        const stashedChunk = new EncodedVideoChunk({
          type: 'key',
          timestamp: performance.now() * 1000,
          data: pendingSharedKeyframe,
        });
        pendingSharedKeyframe = null;
        try {
          decoder.decode(stashedChunk);
          mainDecoderHasKeyframe = true;
        } catch (e) {
          initiateFallback(e, 'main_decoder_decode');
        }
      }
      return true;
    } catch (e) {
      initiateFallback(e, 'main_decoder_configure');
      return false;
    }
  }
  if (!runPreflightChecks()) {
    return;
  }


  const pathname = window.location.pathname.substring(
    0,
    window.location.pathname.lastIndexOf('/') + 1
  );

  async function readLocalClipboardAndSend() {
    if (isSharedMode || !window.clipboard_enabled || !clipboard_in_enabled) return;

    if (!enable_binary_clipboard) {
      navigator.clipboard
        .readText()
        .then((text) => {
          if (!text) return;
          sendClipboardData(text);
          console.log("Sent clipboard text via sendClipboardData");
        })
        .catch((err) => {
          if (err.name !== 'NotFoundError' && !err.message.includes('not focused')) {
             console.warn(`Could not read text clipboard: ${err.name} - ${err.message}`);
          }
        });
    } else {
      try {
        const clipboardItems = await navigator.clipboard.read();
        if (!clipboardItems || clipboardItems.length === 0) {
          return;
        }
        const clipboardItem = clipboardItems[0];
        const imageType = clipboardItem.types.find(type => type.startsWith('image/'));

        if (imageType) {
          const blob = await clipboardItem.getType(imageType);
          const arrayBuffer = await blob.arrayBuffer();
          sendClipboardData(arrayBuffer, imageType);
          console.log(`Sent binary clipboard via sendClipboardData: ${imageType}, size: ${blob.size} bytes`);
        } else if (clipboardItem.types.includes('text/plain')) {
          const blob = await clipboardItem.getType('text/plain');
          const text = await blob.text();
          if (!text) return;
          sendClipboardData(text);
          console.log("Sent clipboard text (from binary-enabled path) via sendClipboardData");
        }
      } catch (err) {
        if (err.name !== 'NotFoundError' && !err.message.includes('not focused')) {
          console.warn(`Could not read clipboard using advanced API: ${err.name} - ${err.message}`);
        }
      }
    }
  }

  // Chromium reads the clipboard on focus without friction. Firefox/WebKit raise an
  // intrusive paste prompt on every focus read, so there the read is driven only by
  // the Ctrl/Cmd+V keydown and paste-event handlers below.
  if (isChromium) {
    window.addEventListener('focus', () => { readLocalClipboardAndSend(); });
  }

  // Fallback for browsers that reject navigator.clipboard.write (older Firefox/Safari):
  // execCommand('copy') from a hidden textarea. A last resort, since awaiting the
  // promise first can outlive the Ctrl/Cmd+C transient activation.
  async function execCommandCopyFallback(textPromise) {
    let text = '';
    try { text = await textPromise; } catch (_) { text = lastServerClipboardText || ''; }
    if (typeof text !== 'string') text = lastServerClipboardText || '';
    // Don't clobber the user's local clipboard with empty content (slow/empty server
    // response on the first copy of a session).
    if (!text) return;
    const ta = document.createElement('textarea');
    ta.value = text;
    ta.setAttribute('readonly', '');
    ta.style.position = 'fixed';
    ta.style.top = '-9999px';
    ta.style.left = '-9999px';
    ta.style.opacity = '0';
    document.body.appendChild(ta);
    try {
      ta.focus();
      ta.select();
      ta.setSelectionRange(0, ta.value.length);
      const ok = document.execCommand('copy');
      if (!ok) console.warn('execCommand("copy") fallback returned false.');
    } catch (err) {
      console.warn(`execCommand("copy") fallback threw: ${err && err.name} - ${err && err.message}`);
    } finally {
      document.body.removeChild(ta);
    }
  }

  // Safari/Firefox reject navigator.clipboard from the focus/message handlers
  // (no transient activation), so for those browsers mirror the sync onto the
  // Ctrl/Cmd+V (read) and Ctrl/Cmd+C (write) key gestures. Chrome keeps using
  // the focus/message path untouched. Never preventDefault: the keystroke must
  // still reach the remote session.
  if (!isChromium) {
    window.addEventListener('keydown', (event) => {
      if (isSharedMode || !window.clipboard_enabled) return;
      if (!(event.ctrlKey || event.metaKey) || event.altKey) return;
      // Once per physical keypress: autorepeat must not spam REQUEST_CLIPBOARD / reads.
      if (event.repeat) return;
      // Only drive remote-clipboard sync from the stream; don't hijack copy/paste in
      // page form fields (settings UI, etc.). The stream's overlay input is exempt.
      const ae = document.activeElement;
      if (ae && ae.id !== 'overlayInput' &&
          (ae.tagName === 'INPUT' || ae.tagName === 'TEXTAREA' || ae.tagName === 'SELECT' || ae.isContentEditable)) {
        return;
      }
      const key = (event.key || '').toLowerCase();
      // Read (Ctrl/Cmd+V) is handled by the 'paste' listener below via event.clipboardData:
      // synchronous, no Firefox paste-prompt, and no double-send. Reading here through
      // navigator.clipboard.readText() would re-raise the prompt and send twice.
      if (key === 'c' && clipboard_out_enabled) {
        // Advertise text/plain ONLY: a Ctrl/Cmd+C can't synchronously know whether the
        // server's CURRENT clipboard is an image, and a stale lastServerClipboardMime
        // would build a malformed ClipboardItem (image entry holding text). Server images
        // are delivered by the push handler instead.
        const textPromise = requestServerClipboard(false);
        const items = {
          'text/plain': textPromise.then((t) =>
            new Blob([typeof t === 'string' ? t : (lastServerClipboardText || '')], { type: 'text/plain' }))
        };
        let writePromise = null;
        try {
          writePromise = navigator.clipboard.write([new ClipboardItem(items)]);
        } catch (err) {
          // Synchronous throw (e.g. ClipboardItem/clipboard.write unsupported).
          console.warn(`navigator.clipboard.write unavailable on Ctrl+C, using execCommand: ${err && err.name}`);
          execCommandCopyFallback(textPromise);
        }
        if (writePromise && writePromise.catch) {
          writePromise.catch((err) => {
            console.warn(`navigator.clipboard.write rejected on Ctrl+C, using execCommand: ${err && err.name} - ${err && err.message}`);
            execCommandCopyFallback(textPromise);
          });
        }
      }
    }, true);

    // The 'v' keydown path reads via navigator.clipboard.read()/readText(), which
    // WebKit/Safari reject with NotAllowedError even with an editable focused. The
    // 'paste' event exposes event.clipboardData synchronously in both WebKit and
    // Firefox, so drive paste-to-server from it there (the stream's overlayInput is
    // the focused editable target, so the event fires and bubbles to the window).
    // Don't preventDefault: the paste chord must still reach the remote session.
    window.addEventListener('paste', (event) => {
      if (isSharedMode || !window.clipboard_enabled || !clipboard_in_enabled) return;
      // Only drive remote-clipboard sync from the stream; don't hijack paste into
      // page form fields (settings UI, etc.). The stream's overlay input is exempt.
      const ae = document.activeElement;
      if (ae && ae.id !== 'overlayInput' &&
          (ae.tagName === 'INPUT' || ae.tagName === 'TEXTAREA' || ae.tagName === 'SELECT' || ae.isContentEditable)) {
        return;
      }
      const cd = event.clipboardData;
      if (!cd) return;
      // Prefer an image when binary clipboard is on and the payload carries one.
      if (enable_binary_clipboard && cd.items) {
        for (let i = 0; i < cd.items.length; i++) {
          const it = cd.items[i];
          if (it.kind === 'file' && it.type && it.type.startsWith('image/')) {
            const file = it.getAsFile();
            if (file) {
              file.arrayBuffer()
                .then((buf) => sendClipboardData(buf, it.type))
                .catch((err) => console.warn(`Paste image read failed: ${err && err.name}`));
              return;
            }
          }
        }
      }
      const text = cd.getData('text/plain');
      if (text) sendClipboardData(text);
    }, true);
  }

  document.addEventListener('visibilitychange', async () => {
    if (isSharedMode) {
      // A shared viewer can't stop the pipeline, but the server can pause
      // just THIS socket's video broadcast: report tab visibility so a
      // hidden viewer stops costing stream bandwidth. On return the server
      // answers START_VIDEO with the same reset + IDR a freshly joining
      // viewer gets, so the picture resumes on the next keyframe.
      if (websocket && websocket.readyState === WebSocket.OPEN) {
        websocket.send(document.hidden ? 'STOP_VIDEO' : 'START_VIDEO');
        console.log(`Shared mode: tab ${document.hidden ? 'hidden, requested video pause' : 'visible, requested video resume'}.`);
      }
      return;
    }
    if (document.hidden) {
      console.log('Tab is hidden, stopping video pipeline if active.');
      if (websocket && websocket.readyState === WebSocket.OPEN) {
        if (isVideoPipelineActive) {
          websocket.send('STOP_VIDEO');
          isVideoPipelineActive = false;
          window.postMessage({ type: 'pipelineStatusUpdate', video: false }, window.location.origin);
          console.log("Tab hidden: Sent STOP_VIDEO. Clearing canvas visually. Server will send PIPELINE_RESETTING for full state reset.");
          if (canvasContext && canvas) {
              try {
                  canvasContext.setTransform(1, 0, 0, 1, 0, 0);
                  canvasContext.clearRect(0, 0, canvas.width, canvas.height);
              } catch (e) { console.error("Error clearing canvas on tab hidden:", e); }
          }
        }
      }
    } else {
      console.log('Tab is visible, requesting video pipeline start if it was inactive.');
      if (currentEncoderMode !== 'jpeg' && currentEncoderMode !== 'h264enc' && currentEncoderMode !== 'openh264enc' && currentEncoderMode !== 'h264enc-striped') {
          console.log('Tab visible: Re-initializing VideoDecoder to recover from background reclamation.');
          triggerInitializeDecoder(); 
      }
      if (websocket && websocket.readyState === WebSocket.OPEN) {
        if (!isVideoPipelineActive) {
          websocket.send('START_VIDEO');
          if (wakeLockSentinel === null) {
            console.log('Tab is visible again, re-acquiring Wake Lock.');
            await requestWakeLock();
          }
          isVideoPipelineActive = true;
          // START_VIDEO can be lost (server never restarts encode -> black stream);
          // watch for the first VIDEO_STARTED / video chunk and recover if none lands.
          armStartVideoWatchdog();
          window.postMessage({ type: 'pipelineStatusUpdate', video: true }, window.location.origin);
          console.log("Tab visible: Sent START_VIDEO. Clearing canvas visually. Server will send PIPELINE_RESETTING for full state reset.");
          if (canvasContext && canvas) {
            try {
                canvasContext.setTransform(1, 0, 0, 1, 0, 0);
                canvasContext.clearRect(0, 0, canvas.width, canvas.height);
            } catch (e) { console.error("Error clearing canvas on tab visible/start:", e); }
          }
        }
      }
    }
  });

  async function decodeAndQueueJpegStripe(startY, jpegData, frameId) {
    try {
      // ImageDecoder (WebCodecs) is the primary path, but it needs a secure context.
      // Over plain http, fall back to createImageBitmap, which decodes JPEG anywhere.
      // Both yield a drawable/closeable image the render + cleanup paths handle alike.
      let image;
      if (typeof ImageDecoder !== 'undefined') {
        const imageDecoder = new ImageDecoder({ data: jpegData, type: 'image/jpeg' });
        image = (await imageDecoder.decode()).image;
        imageDecoder.close();
      } else if (typeof createImageBitmap === 'function') {
        image = await createImageBitmap(new Blob([jpegData], { type: 'image/jpeg' }));
      } else {
        console.warn('No JPEG decoder available (ImageDecoder and createImageBitmap both missing).');
        return;
      }
      jpegStripeRenderQueue.push({ image, startY, frameId });
    } catch (error) {
      console.error('Error decoding JPEG stripe:', error, 'startY:', startY, 'dataLength:', jpegData.byteLength);
    }
  }

  function handleDecodedFrame(frame) {
    // Frames arriving from the main VideoDecoder: shared mode, plus any full-frame mode that
    // isn't jpeg/h264enc/openh264enc/h264enc-striped (those use the JPEG and per-stripe decoder paths).
    const isMainDecoderMode = isSharedMode || (currentEncoderMode !== 'jpeg' && currentEncoderMode !== 'h264enc-striped' && currentEncoderMode !== 'h264enc' && currentEncoderMode !== 'openh264enc');

    if (document.hidden && isMainDecoderMode) {
      frame.close();
      return;
    }

    if (!isSharedMode && clientMode === 'websockets' && !isVideoPipelineActive) {
      frame.close();
      return;
    }

    if (isSharedMode) {
        const physicalFrameWidth = frame.displayWidth;
        const physicalFrameHeight = frame.displayHeight;

        if ((manual_width !== physicalFrameWidth || manual_height !== physicalFrameHeight) && physicalFrameWidth > 0 && physicalFrameHeight > 0) { 
            manual_width = physicalFrameWidth;
            manual_height = physicalFrameHeight;
            console.log(`Shared mode (decoded H264): Updated dimensions from H.264 frame to ${manual_width}x${manual_height} (Physical)`);
            applyManualCanvasStyle(manual_width, manual_height, true);
        }
    }

    if (isMainDecoderMode) {
      // Render-on-decode: present the freshest frame the instant it decodes (lowest
      // glass-to-glass latency) instead of waiting for the next rAF. presentFrameToVideo
      // (Chromium main-thread MSTG) and presentFrameToWorker (worker VTG, else OffscreenCanvas)
      // drop on backpressure and deactivate to canvas on error. Anything not consumed (no
      // sink, or the worker still handshaking) goes to the rAF/canvas buffer.
      if (!isSharedMode && supportsWindowMSTG && presentFrameToVideo(frame)) {
        // handed straight to the main-thread <video> track generator
      } else if (!isSharedMode && USE_OFFSCREEN_WORKER && presentFrameToWorker(frame)) {
        // handed to the worker sink (VideoTrackGenerator <video>, or OffscreenCanvas)
      } else {
        videoFrameBuffer.push(frame);
      }
    } else {
      console.warn(`[handleDecodedFrame] Frame received but not for a main-decoder mode that uses videoFrameBuffer. isSharedMode: ${isSharedMode}, currentEncoderMode: ${currentEncoderMode}. Closing frame to be safe.`);
      frame.close();
    }
  }

  triggerInitializeDecoder = initializeDecoder;
  console.log("initializeDecoder function assigned to triggerInitializeDecoder.");

  function paintVideoFrame() {
    if (!canvas || !canvasContext) {
      requestAnimationFrame(paintVideoFrame);
      return;
    }

    // Leaving a full-frame mode (now striped/JPEG)? hand rendering back to canvas.
    // Hoisted so both the track-generator (MSTG) and OffscreenCanvas worker sinks
    // are torn down symmetrically; otherwise a worker canvas (Firefox) stays shown
    // covering the real striped/JPEG content after an H.264->JPEG switch or reset.
    if (mstgActive || videoWorkerActive) {
      const fullFrameMode = isSharedMode || (currentEncoderMode !== 'jpeg' && currentEncoderMode !== 'h264enc-striped');
      if (mstgActive && !fullFrameMode) deactivateMstg();
      if (videoWorkerActive && !fullFrameMode) deactivateVideoWorker();
    }

    const dpr = (isSharedMode) ? 1 : (window.devicePixelRatio || 1);

    if (isSharedMode) {
      if (manual_width && manual_height && manual_width > 0 && manual_height > 0) {
          const expectedPhysicalCanvasWidth = alignResolution(manual_width * dpr);
          const expectedPhysicalCanvasHeight = alignResolution(manual_height * dpr);
          if (canvas.width !== expectedPhysicalCanvasWidth || canvas.height !== expectedPhysicalCanvasHeight) {
            console.log(`Shared mode (paintVideoFrame): Canvas buffer ${canvas.width}x${canvas.height} out of sync with expected physical ${expectedPhysicalCanvasWidth}x${expectedPhysicalCanvasHeight} (logical: ${manual_width}x${manual_height}). Re-applying style.`);
            applyManualCanvasStyle(manual_width, manual_height, true);
          }
      }
    }

    let videoPaintedThisFrame = false;
    let jpegPaintedThisFrame = false;

    if (!isSharedMode && (currentEncoderMode === 'h264enc' || currentEncoderMode === 'openh264enc')) {
      // Full-frame H.264 (NVENC/x264 'h264enc', OpenH264 'openh264enc'): present the
      // freshest frame via the zero-copy <video> track generator (Chromium/Safari) or
      // the OffscreenCanvas worker (Firefox), falling back to the 2D canvas. One
      // full frame per decode, so drop older queued frames and present only the newest.
      let paintedSomethingThisCycle = false;
      if (decodedStripesQueue.length > 0) {
        // Drop all older queued frames and present only the newest. Index math instead of
        // repeated Array.shift() (each shift() re-indexes the whole array -> O(n^2) on a burst).
        const lastIdx = decodedStripesQueue.length - 1;
        for (let i = 0; i < lastIdx; i++) {
          try { decodedStripesQueue[i].frame.close(); } catch (e) {}
        }
        const frame = decodedStripesQueue[lastIdx].frame;
        decodedStripesQueue.length = 0;  // single truncation, no per-element reindex
        if (supportsWindowMSTG && presentFrameToVideo(frame)) {
          // handed to the main-thread <video> track generator (zero-copy)
        } else if (USE_OFFSCREEN_WORKER && presentFrameToWorker(frame)) {
          // handed to the worker sink (VideoTrackGenerator <video>, or OffscreenCanvas)
        } else {
          if (canvas.width > 0 && canvas.height > 0) {
            canvasContext.drawImage(frame, 0, 0);
          }
          try { frame.close(); } catch (e) {}
        }
        paintedSomethingThisCycle = true;
      }
      if (paintedSomethingThisCycle && !streamStarted) {
        startStream();
      }
    } else if (!isSharedMode && currentEncoderMode === 'h264enc-striped') {
      // Striped H.264: composite stripes onto the 2D canvas (a track-generator
      // <video> can't composite partial-height stripes).
      let paintedSomethingThisCycle = false;
      for (const stripeData of decodedStripesQueue) {
        if (canvas.width > 0 && canvas.height > 0) {
            canvasContext.drawImage(stripeData.frame, 0, stripeData.yPos);
        }
        stripeData.frame.close();
        paintedSomethingThisCycle = true;
      }
      decodedStripesQueue = [];
      if (paintedSomethingThisCycle && !streamStarted) {
        startStream();
      }
    } else if (!isSharedMode && currentEncoderMode === 'jpeg') {
      if (canvasContext && jpegStripeRenderQueue.length > 0) {
        if ((canvas.width === 0 || canvas.height === 0) || (canvas.width === 300 && canvas.height === 150)) {
          const firstStripe = jpegStripeRenderQueue[0];
          if (firstStripe && firstStripe.image && (firstStripe.startY + firstStripe.image.height > canvas.height || firstStripe.image.width > canvas.width)) {
            console.warn(`[paintVideoFrame] Canvas dimensions (${canvas.width}x${canvas.height}) may be too small for JPEG stripes.`);
          }
        }
        while (jpegStripeRenderQueue.length > 0) {
          const segment = jpegStripeRenderQueue.shift();
          if (segment && segment.image) {
            // Skip stripes that finished decoding out of order, i.e. trailing the last drawn
            // id by a small window. A larger modular gap is a fresh stripe after a long static
            // stretch (or a uint16 wrap), so draw it rather than wedge the row.
            const segFrameId = segment.frameId;
            const lastDrawn = lastDrawnJpegStripeFrameId[segment.startY];
            if (segFrameId !== undefined && lastDrawn !== undefined) {
              const behindBy = (lastDrawn - segFrameId) & 0xFFFF;
              const isOlder = behindBy > 0 && behindBy <= JPEG_STRIPE_REORDER_WINDOW;
              if (isOlder) {
                try { segment.image.close(); } catch (closeError) { /* ignore */ }
                continue;
              }
            }
            try {
              if (canvas.width > 0 && canvas.height > 0) {
                canvasContext.drawImage(segment.image, 0, segment.startY);
              }
              if (segFrameId !== undefined) {
                lastDrawnJpegStripeFrameId[segment.startY] = segFrameId;
              }
              segment.image.close();
              jpegPaintedThisFrame = true;
            } catch (e) {
              console.error("[paintVideoFrame] Error drawing JPEG segment:", e, segment);
              if (segment.image && typeof segment.image.close === 'function') {
                try { segment.image.close(); } catch (closeError) { /* ignore */ }
              }
            }
          }
        }
        if (jpegPaintedThisFrame) {
          frameCount++;
          if (!streamStarted) {
            startStream();
            if (!inputInitialized && !isSharedMode) initializeInput();
          }
        }
      }
    } else if ( isSharedMode || (!isSharedMode && currentEncoderMode !== 'jpeg' && currentEncoderMode !== 'h264enc' && currentEncoderMode !== 'openh264enc' && currentEncoderMode !== 'h264enc-striped') ) {
      if (!document.hidden || (isSharedMode && sharedClientState === 'ready')) {
        if ( (isSharedMode && sharedClientState === 'ready') || (!isSharedMode && isVideoPipelineActive) ) {
           if (videoFrameBuffer.length === 0 && videoPaintedSinceLastTick) {
                // A live stream painted last tick but has nothing now: a late frame. Hold a
                // one-frame cushion for a while so jitter stops surfacing as stalls.
                videoPaintedSinceLastTick = false;
                lastVideoUnderrunTime = performance.now();
                window.selkiesVideoStats.underruns++;
           }
           if (videoFrameBuffer.length > 0) {
                // Full-frame H.264: close everything older than the adaptive cushion, paint
                // the oldest of what remains. Draining one-per-rAF would let a burst back up
                // the decoder's bounded output pool; presenting only the newest turns arrival
                // jitter into stalls on slow decoders — so a one-frame cushion is kept ONLY
                // while underruns are recent. Index math avoids O(n^2) Array.shift().
                const cushion =
                    (performance.now() - lastVideoUnderrunTime < VIDEO_CUSHION_HOLD_MS) ? 1 : 0;
                window.selkiesVideoStats.cushion = cushion;
                const keep = Math.min(videoFrameBuffer.length, cushion + 1);
                const firstKept = videoFrameBuffer.length - keep;
                for (let i = 0; i < firstKept; i++) { try { videoFrameBuffer[i]?.close(); } catch (e) {} }
                const frameToPaint = videoFrameBuffer[firstKept];
                videoFrameBuffer = videoFrameBuffer.slice(firstKept + 1);
                videoPaintedSinceLastTick = true;
                if (frameToPaint) {
                    // Shared viewers keep the jitter cushion above but present through the
                    // same zero-copy sink; the <video> box mirrors the shared canvas geometry
                    // (applyManualCanvasStyle marks it dirty) and falls back to canvas below.
                    if (supportsWindowMSTG && presentFrameToVideo(frameToPaint)) {
                        // frame handed to the main-thread <video> track (or closed on failure)
                    } else if (USE_OFFSCREEN_WORKER && presentFrameToWorker(frameToPaint)) {
                        // frame handed to the worker sink (VideoTrackGenerator <video>, or OffscreenCanvas)
                    } else {
                        if (canvas.width > 0 && canvas.height > 0) {
                            canvasContext.drawImage(frameToPaint, 0, 0);
                        }
                        frameToPaint.close();
                    }
                    videoPaintedThisFrame = true;
                    frameCount++;
                    if (!streamStarted) {
                        startStream();
                        if (!inputInitialized && !isSharedMode) initializeInput();
                    }
                }
            }
        }
      }
    }
    requestAnimationFrame(paintVideoFrame);
  }

  async function initializeAudio() {
    if (displayId !== 'primary') {
        console.log("Secondary display: Audio pipeline initialization skipped.");
        return;
    }

    if (window.isAudioInitializing) return;
    window.isAudioInitializing = true;

    try {
      if (audioDecoderWorker) {
      console.warn("Terminating existing audio worker during init.");
      audioDecoderWorker.terminate();
      audioDecoderWorker = null;
    }
    if (audioContext) {
      console.warn("Closing existing AudioContext during init.");
      try { await audioContext.close(); } catch (e) { console.error(e); }
      audioContext = null;
      audioWorkletNode = null;
      audioWorkletProcessorPort = null;
    }
    if (!audioContext) {
      const contextOptions = {
        sampleRate: 48000
      };
      audioContext = new(window.AudioContext || window.webkitAudioContext)(contextOptions);
      console.log('Playback AudioContext initialized. Actual sampleRate:', audioContext.sampleRate, 'Initial state:', audioContext.state);
      audioContext.onstatechange = () => {
        if (!audioContext) return; 
        
        console.log(`Playback AudioContext state changed to: ${audioContext.state}`);
        if (audioContext.state === 'running') {
          applyOutputDevice();
        }
      };
    }
    try {
      const audioWorkletProcessorCode = `
        class AudioFrameProcessor extends AudioWorkletProcessor {
            constructor(options) {
                super();
                this.channels = (options && options.processorOptions && options.processorOptions.channels) || 2;
                this.audioBufferQueue = [];
                this.currentAudioData = null;
                this.currentDataOffset = 0;

                this.TARGET_BUFFER_PACKETS = 3;
                this.MAX_BUFFER_PACKETS = 8;

                // Concealment counters: zero-filled samples output on underrun, and
                // packets dropped by the drop-oldest ring when the queue overflows.
                this.underrunSamples = 0;
                this.droppedOldest = 0;

                this.port.onmessage = (event) => {
                    if (event.data.audioData) {
                        const pcmData = new Float32Array(event.data.audioData);
                        if (this.audioBufferQueue.length >= this.MAX_BUFFER_PACKETS) {
                            this.audioBufferQueue.shift();
                            this.droppedOldest++;
                        }
                        this.audioBufferQueue.push(pcmData);
                    } else if (event.data.type === 'getBufferSize') {
                        const bufferMillis = this.audioBufferQueue.reduce((total, buf) => total + (buf.length / this.channels / sampleRate) * 1000, 0);
                        this.port.postMessage({
                            type: 'audioBufferSize',
                            size: this.audioBufferQueue.length,
                            durationMs: bufferMillis,
                            underrunSamples: this.underrunSamples,
                            droppedOldest: this.droppedOldest
                        });
                    }
                };
            }

            process(inputs, outputs, parameters) {
                const output = outputs[0];
                if (!output || !output[0]) {
                    return true;
                }
                // The decoder hands interleaved f32 data with this.channels channels;
                // de-interleave into however many output channels were configured.
                const chans = output.length;
                const samplesPerBuffer = output[0].length;
                const zeroFill = (from) => {
                    for (let c = 0; c < chans; c++) output[c].fill(0, from);
                };

                if (this.audioBufferQueue.length === 0 && this.currentAudioData === null) {
                    zeroFill(0);
                    this.underrunSamples += samplesPerBuffer;   // full-buffer concealment
                    return true;
                }

                let data = this.currentAudioData;
                let offset = this.currentDataOffset;

                for (let sampleIndex = 0; sampleIndex < samplesPerBuffer; sampleIndex++) {
                    if (!data || offset >= data.length) {
                        if (this.audioBufferQueue.length > 0) {
                            data = this.currentAudioData = this.audioBufferQueue.shift();
                            offset = this.currentDataOffset = 0;
                        } else {
                            this.currentAudioData = null;
                            this.currentDataOffset = 0;
                            zeroFill(sampleIndex);
                            this.underrunSamples += (samplesPerBuffer - sampleIndex);   // partial concealment
                            return true;
                        }
                    }

                    for (let c = 0; c < chans; c++) {
                        output[c][sampleIndex] = offset < data.length ? data[offset++] : output[0][sampleIndex];
                    }
                }

                this.currentDataOffset = offset;
                if (data && offset >= data.length) {
                    this.currentAudioData = null;
                    this.currentDataOffset = 0;
                }

                return true;
            }
        }
        registerProcessor('audio-frame-processor', AudioFrameProcessor);
      `;
      const audioWorkletBlob = new Blob([audioWorkletProcessorCode], {
        type: 'text/javascript'
      });
      const audioWorkletURL = URL.createObjectURL(audioWorkletBlob);
      await audioContext.audioWorklet.addModule(audioWorkletURL);
      URL.revokeObjectURL(audioWorkletURL);
      const workletChannels = getAudioChannelCount();
      if (workletChannels > 2) {
        // Best effort: raise the destination width so surround isn't downmixed
        // before the device (the browser still downmixes to the device's layout).
        try {
          audioContext.destination.channelCount = Math.min(
            workletChannels, audioContext.destination.maxChannelCount || workletChannels);
        } catch (e) {
          console.warn('Could not widen audio destination:', e);
        }
      }
      audioWorkletNode = new AudioWorkletNode(audioContext, 'audio-frame-processor', {
        numberOfOutputs: 1,
        outputChannelCount: [workletChannels],
        processorOptions: { channels: workletChannels }
      });
      audioWorkletProcessorPort = audioWorkletNode.port;
      audioWorkletProcessorPort.onmessage = (event) => {
        if (event.data.type === 'audioBufferSize') {
            window.currentAudioBufferSize = event.data.size;
            window.currentAudioBufferDuration = event.data.durationMs;
            if (event.data.underrunSamples !== undefined) {
              window.currentAudioUnderrunSamples = event.data.underrunSamples;
            }
            if (event.data.droppedOldest !== undefined) {
              window.currentAudioWorkletDropped = event.data.droppedOldest;
            }
        }
      };
      audioGainNode = audioContext.createGain();
      audioGainNode.gain.value = currentVolume;
      audioWorkletNode.connect(audioGainNode);
      audioGainNode.connect(audioContext.destination);
      console.log('Playback AudioWorkletProcessor initialized and connected through a GainNode for volume control.');
      await applyOutputDevice();
      await applyOutputDevice();

      if (audioDecoderWorker) {
        console.warn("[Main] Terminating existing audio decoder worker before creating a new one.");
        audioDecoderWorker.postMessage({
          type: 'close'
        });
        await new Promise(resolve => setTimeout(resolve, 50));
        if (audioDecoderWorker) audioDecoderWorker.terminate();
        audioDecoderWorker = null;
      }
      const audioDecoderWorkerBlob = new Blob([audioDecoderWorkerCode], {
        type: 'application/javascript'
      });
      const audioDecoderWorkerURL = URL.createObjectURL(audioDecoderWorkerBlob);
      audioDecoderWorker = new Worker(audioDecoderWorkerURL);
      URL.revokeObjectURL(audioDecoderWorkerURL);
      audioDecoderWorker.onmessage = (event) => {
        const {
          type,
          reason,
          message
        } = event.data;
        if (type === 'decoderInitFailed') {
          console.error(`[Main] Audio Decoder Worker failed to initialize: ${reason}`);
        } else if (type === 'decoderError') {
          console.error(`[Main] Audio Decoder Worker reported error: ${message}`);
        } else if (type === 'decoderInitialized') {
          console.log('[Main] Audio Decoder Worker confirmed its decoder is initialized.');
        } else if (type === 'decodedAudioData') {
          const pcmBufferFromWorker = event.data.pcmBuffer;
          if (pcmBufferFromWorker && audioWorkletProcessorPort && audioContext && audioContext.state === 'running') {
            if (window.currentAudioBufferSize < 10) {
              audioWorkletProcessorPort.postMessage({
                audioData: pcmBufferFromWorker
              }, [pcmBufferFromWorker]);
            }
          }
        }
      };
      audioDecoderWorker.onerror = (error) => {
        console.error('[Main] Uncaught error in Audio Decoder Worker:', error.message, error);
        if (audioDecoderWorker) {
          audioDecoderWorker.terminate();
          audioDecoderWorker = null;
        }
      };
      if (audioWorkletProcessorPort) {
        const initChannels = getAudioChannelCount();
        audioDecoderWorker.postMessage({
          type: 'init',
          data: {
            initialPipelineStatus: isAudioPipelineActive,
            channels: initChannels,
            description: initChannels > 2 ? buildMultiopusDescription(initChannels) : null
          }
        });
        console.log('[Main] Audio Decoder Worker created and init message sent.');
      } else {
        console.error("[Main] audioWorkletProcessorPort is null, cannot initialize audioDecoderWorker correctly.");
      }
    } catch (error) {
      console.error('Error initializing Playback AudioWorklet:', error);
      if (audioContext && audioContext.state !== 'closed') {
        audioContext.close();
      }
      audioContext = null;
      audioWorkletNode = null;
      audioWorkletProcessorPort = null;
    }
    } finally {
      window.isAudioInitializing = false;
    }
  }

  async function initializeDecoderAudio() {
    if (audioDecoderWorker) {
      console.log('[Main] Requesting Audio Decoder Worker to reinitialize its decoder.');
      audioDecoderWorker.postMessage({
        type: 'reinitialize'
      });
    } else {
      console.warn('[Main] Cannot initialize decoder audio: Audio Decoder Worker not available. Call initializeAudio() first.');
      if (clientMode === 'websockets' && !audioContext) {
        console.log('[Main] Audio context missing, attempting to initialize full audio pipeline for websockets.');
        await initializeAudio();
      }
    }
  }

  const ws_protocol = location.protocol === 'http:' ? 'ws://' : 'wss://';
  let websocketEndpointURL = new URL(`${ws_protocol}${window.location.host}${pathname}`);
  if (isTokenAuthMode) {
      websocketEndpointURL.search = `?token=${authToken}`;
  } else if (isSharedMode) {
      // Pass role/slot as query params so the server can assign permissions
      // (URL fragments are never transmitted to the server per HTTP spec)
      const wsParams = new URLSearchParams();
      wsParams.set('role', 'viewer');
      if (detectedSharedModeType && detectedSharedModeType.startsWith('player')) {
          const playerSlot = detectedSharedModeType.replace('player', '');
          if (playerSlot >= 2 && playerSlot <= 4) {
              wsParams.set('slot', playerSlot);
          }
      }
      websocketEndpointURL.search = wsParams.toString();
  }
  websocketEndpointURL.pathname += 'websockets';

  websocket = new WebSocket(websocketEndpointURL.href);
  websocket.binaryType = 'arraybuffer';

  const sendBackpressureAck = () => {
    if (websocket && websocket.readyState === WebSocket.OPEN) {
      try {
        if (lastReceivedVideoFrameId !== -1) {
          websocket.send(`CLIENT_FRAME_ACK ${lastReceivedVideoFrameId}`);
        }
      } catch (error) {
        console.error('[Backpressure] Error sending frame ACK:', error);
      }
    }
  };

  const sendClientMetrics = () => {
    if (isSharedMode) return;

    // Refresh audio buffer depth every interval so backpressure gates work even when the sidebar is closed.
    if (audioWorkletProcessorPort) {
      audioWorkletProcessorPort.postMessage({
        type: 'getBufferSize'
      });
    }

    if (isSidebarOpen) {
      const now = performance.now();
      const elapsedStriped = now - lastStripedFpsUpdateTime;
      const elapsedFullFrame = now - lastFpsUpdateTime;
      const fpsUpdateInterval = 1000;

      if (uniqueStripedFrameIdsThisPeriod.size > 0) {
        if (elapsedStriped >= fpsUpdateInterval) {
          const stripedFps = (uniqueStripedFrameIdsThisPeriod.size * 1000) / elapsedStriped;
          window.fps = Math.round(stripedFps);
          uniqueStripedFrameIdsThisPeriod.clear();
          lastStripedFpsUpdateTime = now;
          frameCount = 0;
          lastFpsUpdateTime = now;
        }
      } else if (frameCount > 0) {
        if (elapsedFullFrame >= fpsUpdateInterval) {
          const fullFrameFps = (frameCount * 1000) / elapsedFullFrame;
          window.fps = Math.round(fullFrameFps);
          frameCount = 0;
          lastFpsUpdateTime = now;
          lastStripedFpsUpdateTime = now;
        }
      } else {
        if (elapsedStriped >= fpsUpdateInterval || elapsedFullFrame >= fpsUpdateInterval) {
             window.fps = 0;
             lastFpsUpdateTime = now;
             lastStripedFpsUpdateTime = now;
        }
      }
    }
  };

  websocket.onopen = () => {
    console.log('[websockets] Connection opened!');
    status = 'connected_waiting_mode';
    loadingText = 'Connection established. Waiting for server mode...';
    updateStatusDisplay();
    // Advertise gzip support so the server may send large control text (cursor
    // PNGs, clipboard, stats) as 0x05 gzip frames. Small/latency-critical messages
    // stay uncompressed regardless. Browsers without DecompressionStream never opt in.
    if (typeof DecompressionStream !== 'undefined') {
      try { websocket.send('_gz,1'); } catch (e) { /* handshake is best-effort */ }
    }
    window.postMessage({ type: 'trackpadModeUpdate', enabled: trackpadMode }, window.location.origin);
    if (!isSharedMode) {
      const settingsPrefix = `${storageAppName}_`;
      const settingsToSend = {};
      const dpr = useCssScaling ? 1 : (window.devicePixelRatio || 1);
      const isSetBySpecificKey = {};

      const knownSettings = [
        'framerate', 'video_crf', 'encoder', 'is_manual_resolution_mode',
        'audio_bitrate', 'video_fullcolor', 'video_streaming_mode',
        'jpeg_quality', 'paint_over_jpeg_quality', 'use_cpu', 'video_paintover_crf',
        'video_paintover_burst_frames', 'use_paint_over_quality', 'scaling_dpi',
        'enable_binary_clipboard', 'rate_control_mode', 'video_bitrate',
        'force_aligned_resolution'
      ];
      const booleanSettingKeys = [
        'is_manual_resolution_mode', 'video_fullcolor', 'video_streaming_mode',
        'use_cpu', 'use_paint_over_quality', 'enable_binary_clipboard',
        'force_aligned_resolution'
      ];
      const integerSettingKeys = [
        'framerate', 'video_crf', 'audio_bitrate', 'jpeg_quality',
        'paint_over_jpeg_quality', 'video_paintover_crf',
        'video_paintover_burst_frames', 'scaling_dpi', 'video_bitrate'
      ];

      for (const key in localStorage) {
        if (Object.hasOwnProperty.call(localStorage, key) && key.startsWith(settingsPrefix)) {
          const unprefixedKey = key.substring(settingsPrefix.length);
          const displaySuffix = `_${displayId}`;
          const isSpecific = displayId !== 'primary' && unprefixedKey.endsWith(displaySuffix);
          const baseKey = isSpecific ? unprefixedKey.slice(0, -displaySuffix.length) : unprefixedKey;

          if (!isSpecific && isSetBySpecificKey[baseKey]) {
            continue;
          }
          if (knownSettings.includes(baseKey)) {
            if (!isSpecific && isSetBySpecificKey[baseKey]) {
              continue;
            }
            let value = localStorage.getItem(key);
            if (booleanSettingKeys.includes(baseKey)) {
              value = (value === 'true');
            } else if (integerSettingKeys.includes(baseKey)) {
              value = parseInt(value, 10);
              if (isNaN(value)) continue;
            }
            settingsToSend[baseKey] = value;
            if (isSpecific) {
              isSetBySpecificKey[baseKey] = true;
            }
          }
        }
      }

      if (is_manual_resolution_mode && manual_width != null && manual_height != null) {
        settingsToSend['is_manual_resolution_mode'] = true;
        settingsToSend['manual_width'] = alignResolution(manual_width);
        settingsToSend['manual_height'] = alignResolution(manual_height);
      } else {
        const videoContainer = document.querySelector('.video-container');
        const rect = videoContainer ? videoContainer.getBoundingClientRect() : {
          width: window.innerWidth,
          height: window.innerHeight
        };
        settingsToSend['is_manual_resolution_mode'] = false;
        settingsToSend['initialClientWidth'] = alignResolution(rect.width * dpr);
        settingsToSend['initialClientHeight'] = alignResolution(rect.height * dpr);
      }
 
      settingsToSend['useCssScaling'] = useCssScaling;
      settingsToSend['displayId'] = displayId;
      if (displayId === 'display2') {
          settingsToSend['displayPosition'] = displayPosition;
      }
      // Advertise audio-RED capability so the server enables Opus redundancy for this stream.
      settingsToSend['audioRedundancy'] = true;

      try {
        const settingsJson = JSON.stringify(settingsToSend);
        const message = `SETTINGS,${settingsJson}`;
        websocket.send(message);
        console.log('[websockets] Sent initial settings (resolutions are physical) to server:', settingsToSend);
      } catch (e) {
        console.error('[websockets] Error constructing or sending initial settings:', e);
      }
    } else {
        console.log("Shared mode: WebSocket opened. Waiting for 'MODE websockets' from server to start identification sequence.");
    }
    websocket.send('cr');
    console.log('[websockets] Sent initial clipboard request (cr) to server.');
    isVideoPipelineActive = true;
    isAudioPipelineActive = (displayId === 'primary');
    window.postMessage({
      type: 'pipelineStatusUpdate',
      video: true,
      audio: isAudioPipelineActive
    }, window.location.origin);

    if (!isSharedMode) {
        isMicrophoneActive = false;
        if (metricsIntervalId === null) {
          metricsIntervalId = setInterval(sendClientMetrics, METRICS_INTERVAL_MS);
          console.log(`[websockets] Started sending client metrics every ${METRICS_INTERVAL_MS}ms.`);
        }
        if (backpressureIntervalId === null) {
          backpressureIntervalId = setInterval(sendBackpressureAck, BACKPRESSURE_INTERVAL_MS);
          console.log(`[websockets] Started sending backpressure ACKs every ${BACKPRESSURE_INTERVAL_MS}ms.`);
        }
    }
  };

  // Order-preserving dispatch for gzip'd control frames (opcode 0x05). Inflation is
  // async (DecompressionStream), so control messages route through a promise chain to
  // keep their arrival order (e.g. multipart clipboard chunks); the chain is engaged
  // only while an inflation is actually pending, so the common case stays synchronous.
  // Media frames (video/audio) always dispatch immediately — their own frame IDs order
  // them and the compression never touches them.
  let __wsCtrlChain = Promise.resolve();
  let __wsGzPending = 0;
  const __inflateGz = async (buf) => {
    const stream = new Response(new Blob([buf]).stream().pipeThrough(new DecompressionStream('gzip')));
    return new TextDecoder().decode(await stream.arrayBuffer());
  };

  // Client->server compression: once the server echoes '_gz,1', gzip our large text
  // sends (clipboard) as 0x05 binary frames. Small text (input verbs) and binary
  // (mic/file) are never wrapped, so latency-critical data is untouched. An order-
  // preserving chain keeps multipart clipboard chunks in sequence.
  let wsGzTx = false;
  let __wsSendChain = Promise.resolve();
  let __wsSendPending = 0;
  const __compressGz05 = async (str) => {
    const buf = await new Response(new Blob([str]).stream().pipeThrough(new CompressionStream('gzip'))).arrayBuffer();
    const out = new Uint8Array(buf.byteLength + 1);
    out[0] = 0x05;
    out.set(new Uint8Array(buf), 1);
    return out.buffer;
  };
  const __rawWsSend = websocket.send.bind(websocket);
  websocket.send = (data) => {
    if (wsGzTx && typeof data === 'string' && data.length >= 512) {
      __wsSendPending++;
      __wsSendChain = __wsSendChain.then(async () => {
        try { __rawWsSend(await __compressGz05(data)); }
        catch (e) { __rawWsSend(data); }
        finally { __wsSendPending--; }
      });
    } else if (typeof data === 'string' && __wsSendPending > 0) {
      __wsSendChain = __wsSendChain.then(() => __rawWsSend(data));
    } else {
      __rawWsSend(data);
    }
  };

  const __rawWsMessage = (event) => {
    if (event.data instanceof ArrayBuffer) {
      const arrayBuffer = event.data;
      const dataView = new DataView(arrayBuffer);
      if (arrayBuffer.byteLength < 1) return;
      const dataTypeByte = dataView.getUint8(0);

      // Any video chunk (full H.264, JPEG stripe, or H.264 stripe) proves the pipeline
      // came back after a visibility-triggered START_VIDEO; stand the watchdog down.
      if (startVideoWatchdogTimer !== null &&
          (dataTypeByte === 0 || dataTypeByte === 0x03 || dataTypeByte === 0x04)) {
        clearStartVideoWatchdog();
      }

      if (dataTypeByte === 0) {
        const headerLength = isSharedMode ? 2 : 4;
        if (arrayBuffer.byteLength < headerLength) return;

        const frameTypeFlag = dataView.getUint8(1);
        if (!isSharedMode) lastReceivedVideoFrameId = dataView.getUint16(2, false);
        const videoDataArrayBuffer = arrayBuffer.slice(headerLength);

        const canProcessFullH264 =
          isSharedMode ||
          (!isSharedMode && isVideoPipelineActive && currentEncoderMode !== 'jpeg' && currentEncoderMode !== 'h264enc' && currentEncoderMode !== 'openh264enc' && currentEncoderMode !== 'h264enc-striped');

        if (canProcessFullH264) {
          if (isSharedMode && !sharedClientHasReceivedKeyframe) {
            if (frameTypeFlag === 1) {
              console.log("Shared mode: First keyframe received. Opening the gate for video decoding.");
              sharedClientHasReceivedKeyframe = true;
            } else {
              console.log("Shared mode: Gate is closed. Discarding non-keyframe packet.");
              requestKeyframe();
              return;
            }
          }
          if (decoder && decoder.state === 'configured') {
            const chunkType = frameTypeFlag === 1 ? 'key' : 'delta';
            if (chunkType === 'delta' && !mainDecoderHasKeyframe) {
              requestKeyframe();
              return;
            }
            if (chunkType === 'key') {
              mainDecoderHasKeyframe = true;
              // Make pixelflux's in-band SPS authoritative (Chromium only): adopt
              // its profile/level before decoding this keyframe if it differs from
              // the initial guess. No-op on non-Chromium / parse failure.
              maybeReconfigureMainDecoderFromSps(new Uint8Array(videoDataArrayBuffer));
            }
            const chunk = new EncodedVideoChunk({
              type: frameTypeFlag === 1 ? 'key' : 'delta',
              timestamp: performance.now() * 1000,
              data: videoDataArrayBuffer,
            });
            try {
              decoder.decode(chunk);
            } catch (e) {
              initiateFallback(e, 'main_decoder_decode');
            }
          } else {
            if (!isSharedMode && (!decoder || decoder.state === 'closed' || decoder.state === 'unconfigured')) {
              console.warn(`Main decoder not ready for Full H.264 frame (mode: ${currentEncoderMode}, state: ${decoder ? decoder.state : 'null'}). Attempting init. Frame might be dropped.`);
              initializeDecoder();
            } else if (isSharedMode && (!decoder || decoder.state === 'closed' || decoder.state === 'unconfigured')) {
                 console.error(`Shared mode: Main H.264 decoder not available or not configured when expected. State: ${sharedClientState}. Decoder state: ${decoder ? decoder.state : 'null'}. Entering error state.`);
                 sharedClientState = 'error';
            } else {
              console.warn(`Main decoder exists but not configured (state: ${decoder.state}). Full H.264 frame dropped.`);
            }
          }
        }


      } else if (dataTypeByte === 1) {
        if (displayId !== 'primary') return;
        
        const audioHeaderLength = 2;
        if (arrayBuffer.byteLength < audioHeaderLength) return;

        if ((isAudioPipelineActive || isSharedMode)) {
          if (audioDecoderWorker) {
            if (audioContext && audioContext.state !== 'running') {
              audioContext.resume().catch(e => console.error("Error resuming audio context", e));
            }
            const opusFrames = extractOpusFrames(arrayBuffer);
            for (const opusDataArrayBuffer of opusFrames) {
              if (opusDataArrayBuffer.byteLength === 0) continue;
              if (!isSharedMode && window.currentAudioBufferSize >= 5) {
                window.currentAudioDropped++;
                break;
              }
              audioDecoderWorker.postMessage({
                type: 'decode',
                data: {
                  opusBuffer: opusDataArrayBuffer,
                  timestamp: performance.now() * 1000
                }
              }, [opusDataArrayBuffer]);
            }
          } else {
            console.warn("AudioDecoderWorker not ready. Attempting to initialize audio pipeline.");
            initializeAudio().then(() => {
              if (audioDecoderWorker) {
                const opusFrames = extractOpusFrames(arrayBuffer);
                for (const opusDataArrayBuffer of opusFrames) {
                  if (opusDataArrayBuffer.byteLength === 0) continue;
                  if (!isSharedMode && window.currentAudioBufferSize >= 5) { window.currentAudioDropped++; break; }
                  audioDecoderWorker.postMessage({
                    type: 'decode',
                    data: { opusBuffer: opusDataArrayBuffer, timestamp: performance.now() * 1000 }
                  }, [opusDataArrayBuffer]);
                }
              }
            });
          }
        }


      } else if (dataTypeByte === 0x03) {
        const jpegHeaderLength = isSharedMode ? 4 : 6;
        if (arrayBuffer.byteLength < jpegHeaderLength) return;

        const jpegFrameId = isSharedMode ? 0 : dataView.getUint16(2, false);
        if (!isSharedMode) lastReceivedVideoFrameId = jpegFrameId;
        const stripe_y_start = dataView.getUint16(isSharedMode ? 2 : 4, false);
        const jpegDataBuffer = arrayBuffer.slice(jpegHeaderLength);

        const canProcessJpeg =
          (!isSharedMode && isVideoPipelineActive && currentEncoderMode === 'jpeg');

        if (canProcessJpeg) {
          if (jpegDataBuffer.byteLength === 0) return;
          decodeAndQueueJpegStripe(stripe_y_start, jpegDataBuffer, jpegFrameId);
        }

      } else if (dataTypeByte === 0x04) {
        const EXPECTED_HEADER_LENGTH = 10;
        if (arrayBuffer.byteLength < EXPECTED_HEADER_LENGTH) return;

        const video_frame_type_byte = dataView.getUint8(1);
        const vncFrameID = dataView.getUint16(2, false);
        if (!isSharedMode) {
            lastReceivedVideoFrameId = vncFrameID;
            uniqueStripedFrameIdsThisPeriod.add(lastReceivedVideoFrameId);
        }
        const vncStripeYStart = dataView.getUint16(4, false);
        const stripeWidth = dataView.getUint16(6, false);
        const stripeHeight = dataView.getUint16(8, false);
        const h264Payload = arrayBuffer.slice(EXPECTED_HEADER_LENGTH);

        if (isSharedMode) {
            if (!sharedClientHasReceivedKeyframe) {
                if (video_frame_type_byte === 0x01) {
                    console.log("Shared mode: First keyframe received for h264enc fullframe. Opening the gate.");
                    sharedClientHasReceivedKeyframe = true;
                } else {
                    requestKeyframe();
                    return;
                }
            }
            if (h264Payload.byteLength === 0) return;

            if (decoder && decoder.state === 'configured') {
                const chunkType = (video_frame_type_byte === 0x01) ? 'key' : 'delta';
                if (chunkType === 'delta' && !mainDecoderHasKeyframe) {
                    requestKeyframe();
                    return;
                }
                if (chunkType === 'key') {
                    mainDecoderHasKeyframe = true;
                }
                const chunk = new EncodedVideoChunk({
                    type: chunkType,
                    timestamp: performance.now() * 1000,
                    data: h264Payload
                });
                try {
                    decoder.decode(chunk);
                } catch (e) {
                    initiateFallback(e, 'main_decoder_decode');
                }
            } else {
                if (video_frame_type_byte === 0x01) {
                    pendingSharedKeyframe = h264Payload;
                }
                if (!decoder || decoder.state === 'closed' || decoder.state === 'unconfigured') {
                    triggerInitializeDecoder();
                }
            }
            return;
        }

        // Non-shared full-frame H.264 (h264enc/openh264enc): decode inside the worker
        // (Safari/Firefox) so decode and present stay off the main thread. Falls through to
        // the main-thread stripe decoder while the worker is still handshaking or if worker
        // decode has failed. h264enc-striped composites partial stripes on the 2D canvas,
        // so it always decodes on the main thread.
        if (decodeInWorker && (currentEncoderMode === 'h264enc' || currentEncoderMode === 'openh264enc') && isVideoPipelineActive) {
            if (h264Payload.byteLength === 0) return;
            const workerCodec = getDynamicH264Codec(stripeWidth, stripeHeight, video_fullcolor, framerate);
            if (feedWorkerDecoder(video_frame_type_byte === 0x01, h264Payload, stripeWidth, stripeHeight, workerCodec)) {
                return;
            }
        }

        const canProcessVncStripe =
            (!isSharedMode && isVideoPipelineActive && (currentEncoderMode === 'h264enc' || currentEncoderMode === 'openh264enc' || currentEncoderMode === 'h264enc-striped'));

        if (canProcessVncStripe) {
            if (h264Payload.byteLength === 0) return;

            let decoderInfo = vncStripeDecoders[vncStripeYStart];
            const chunkType = (video_frame_type_byte === 0x01) ? 'key' : 'delta';
            if (chunkType === 'delta' && (!decoderInfo || !decoderInfo.hasReceivedKeyframe)) {
                requestKeyframe();
                return;
            }
            if (!decoderInfo || decoderInfo.decoder.state === 'closed' ||
                (decoderInfo.decoder.state === 'configured' && (decoderInfo.width !== stripeWidth || decoderInfo.height !== stripeHeight))) {

                if(decoderInfo && decoderInfo.decoder.state !== 'closed') {
                    try { decoderInfo.decoder.close(); } catch(e) { console.warn("Error closing old VNC stripe decoder:", e); }
                }

                const newStripeDecoder = new VideoDecoder({
                    output: handleDecodedVncStripeFrame.bind(null, vncStripeYStart),
                    error: (e) => initiateFallback(e, `stripe_decoder_Y=${vncStripeYStart}`)
                });
                const dynamicCodec = getDynamicH264Codec(stripeWidth, stripeHeight, video_fullcolor, framerate);
                const decoderConfig = {
                    codec: dynamicCodec,
                    codedWidth: stripeWidth,
                    codedHeight: stripeHeight,
                    optimizeForLatency: true
                };
                vncStripeDecoders[vncStripeYStart] = {
                    decoder: newStripeDecoder,
                    pendingChunks: [],
                    width: stripeWidth,
                    height: stripeHeight,
                    hasReceivedKeyframe: false
                };
                decoderInfo = vncStripeDecoders[vncStripeYStart];

                VideoDecoder.isConfigSupported(decoderConfig)
                    .then(support => {
                        if (support.supported) {
                            return newStripeDecoder.configure(decoderConfig);
                        } else {
                            console.error(`VNC stripe decoder config not supported for Y=${vncStripeYStart}:`, decoderConfig);
                            delete vncStripeDecoders[vncStripeYStart];
                            return Promise.reject("Config not supported");
                        }
                    })
                    .then(() => {
                        processPendingChunksForStripe(vncStripeYStart);
                    })
                    .catch(e => {
                        console.error(`Error configuring VNC stripe decoder Y=${vncStripeYStart}:`, e);
                        if (vncStripeDecoders[vncStripeYStart] && vncStripeDecoders[vncStripeYStart].decoder === newStripeDecoder) {
                            try { if (newStripeDecoder.state !== 'closed') newStripeDecoder.close(); } catch (_) {}
                            delete vncStripeDecoders[vncStripeYStart];
                        }
                    });
            }

            if (decoderInfo) {
                // Drop deltas on a freshly (re)created decoder that has no keyframe yet.
                if (chunkType === 'delta' && !decoderInfo.hasReceivedKeyframe) {
                    requestKeyframe();
                    return;
                }
                if (chunkType === 'key') {
                    decoderInfo.hasReceivedKeyframe = true;
                }
                const chunkTimestamp = performance.now() * 1000;
                const chunkData = {
                    type: chunkType,
                    timestamp: chunkTimestamp,
                    data: h264Payload
                };
                if (decoderInfo.decoder.state === "configured") {
                    const chunk = new EncodedVideoChunk(chunkData);
                    try {
                        decoderInfo.decoder.decode(chunk);
                    } catch (e) {
                        initiateFallback(e, `stripe_decode_Y=${vncStripeYStart}`);
                    }
                } else if (decoderInfo.decoder.state === "unconfigured" || decoderInfo.decoder.state === "configuring") {
                    decoderInfo.pendingChunks.push(chunkData);
                } else {
                     console.warn(`VNC stripe decoder for Y=${vncStripeYStart} in unexpected state: ${decoderInfo.decoder.state}. Dropping chunk.`);
                }
            }
        }


      } else {
        console.warn('Unknown binary data payload type received:', dataTypeByte);
      }
    } else if (typeof event.data === 'string') {
      if (event.data.startsWith('KILL ')) {
        const reason = event.data.substring(5);
        console.error(`Received KILL message from server: ${reason}`);
        if (reconnectIntervalId) clearInterval(reconnectIntervalId);
        if (websocket) {
            websocket.onclose = () => {};
            websocket.close();
        }
        if (statusDisplayElement) {
            statusDisplayElement.textContent = `Connection Terminated: ${reason}`;
            statusDisplayElement.classList.remove('hidden');
        }
        return;
      }
      if (event.data.startsWith('AUTH_SUCCESS,')) {
        let permissions;
        try {
          const payloadStr = event.data.substring(13);
          permissions = JSON.parse(payloadStr);
        } catch (e) {
          console.error("Failed to parse AUTH_SUCCESS message:", e);
          return;
        }
        clientRole = permissions.role;
        clientSlot = permissions.slot;
        console.log(`Authentication successful. Received Role: ${clientRole}, Slot: ${clientSlot}`);
        window.postMessage({ type: 'clientRoleUpdate', role: clientRole }, window.location.origin);

        if (window.webrtcInput && typeof window.webrtcInput.updateControllerSlot === 'function') {
            window.webrtcInput.updateControllerSlot(clientSlot);
        }

        if (clientRole === 'viewer') {
            console.log("Token-based client is a 'viewer'. Applying shared mode compatibility settings.");
            isSharedMode = true;
            if (window.webrtcInput) {
                window.webrtcInput.setSharedMode(true);
            }
            detectedSharedModeType = 'shared';
            if (clientSlot !== null && clientSlot > 0) {
                playerInputTargetIndex = clientSlot - 1;
            } else {
                playerInputTargetIndex = undefined;
            }
            if (!manual_width || manual_width <= 0 || !manual_height || manual_height <= 0) {
                manual_width = 1280; manual_height = 720;
            }
            applyManualCanvasStyle(manual_width, manual_height, true);
            window.addEventListener('resize', () => {
                if (isSharedMode && manual_width && manual_height && manual_width > 0 && manual_height > 0) {
                    applyManualCanvasStyle(manual_width, manual_height, true);
                }
            });
            updateUIForSharedMode();

            if (initializationComplete) {
                console.log("Post-init sync: Forcing shared mode state because 'MODE websockets' was handled before auth.");
                sharedClientState = 'ready';

                if (websocket && websocket.readyState === WebSocket.OPEN) {
                     websocket.send('STOP_VIDEO');
                     setTimeout(() => {
                        if (websocket && websocket.readyState === WebSocket.OPEN) {
                            websocket.send('START_VIDEO');
                            console.log("Shared mode: Sent START_VIDEO after initial STOP_VIDEO.");
                        }
                    }, 250);
                }
            }
        }
      }
      if (event.data.startsWith('MK_ACCESS,')) {
        const accessLevel = parseInt(event.data.split(',')[1]);
        const hasAccess = (accessLevel === 1);
        console.log(`Received MK_ACCESS update: ${hasAccess}`);
        
        if (window.webrtcInput) {
            if (hasAccess) {
                if (!window.webrtcInput.isInputAttached()) {
                    console.log("MK Access Granted: Attaching input context.");
                    window.webrtcInput.attach_context();
                }
            } else {
                console.log("MK Access Revoked: Detaching input context.");
                window.webrtcInput.detach_context();
            }
        }
      }
      if (event.data.startsWith('ROLE_UPDATE,')) {
        let newPermissions;
        try {
          const payloadStr = event.data.substring(12);
          newPermissions = JSON.parse(payloadStr);
        } catch (e) {
          console.error("Failed to parse ROLE_UPDATE message:", e);
          return;
        }
        console.log(`Received role update. New role: ${newPermissions.role}, New slot: ${newPermissions.slot}`);
        const oldSlot = clientSlot;
        clientRole = newPermissions.role;
        clientSlot = newPermissions.slot;

        if (window.webrtcInput && typeof window.webrtcInput.updateControllerSlot === 'function') {
            window.webrtcInput.updateControllerSlot(clientSlot);
        }

        if (oldSlot !== null && clientSlot === null) {
            if (window.webrtcInput && window.webrtcInput.gamepadManager) {
                console.log("Controller slot revoked, disabling gamepad polling.");
                window.webrtcInput.gamepadManager.disable();
            }
        } else if (oldSlot === null && clientSlot !== null) {
            if (window.webrtcInput && window.webrtcInput.gamepadManager && isGamepadEnabled) {
                console.log("Controller slot granted and global gamepad toggle is ON. Enabling gamepad polling.");
                window.webrtcInput.gamepadManager.enable();
            } else if (window.webrtcInput && window.webrtcInput.gamepadManager) {
                console.log("Controller slot granted, but global gamepad toggle is OFF. Polling remains disabled.");
            }
        }
      }
      if (event.data === 'MODE websockets') {
        clientMode = 'websockets';
        console.log('[websockets] Switched to websockets mode.');
        status = 'initializing';
        loadingText = 'Initializing WebSocket mode...';
        updateStatusDisplay();

        if (!isTokenAuthMode) {
            const hash = window.location.hash;
            if (hash === '#shared') {
                clientRole = 'viewer'; clientSlot = null;
                if (clientSlot !== null) playerInputTargetIndex = clientSlot - 1;
            } else if (hash.startsWith('#player')) {
                clientRole = 'viewer'; clientSlot = parseInt(hash.substring(7), 10) || null;
            } else {
                clientRole = 'controller'; clientSlot = 1;
                clientRole = 'controller';
                clientSlot = 1;
                playerInputTargetIndex = 0;
            }
            console.log(`Legacy mode detected. Role from hash: ${clientRole}, Slot: ${clientSlot}`);
            initializeInput();
        }


        if (decoder && decoder.state !== "closed") {
            try { decoder.close(); } catch(e){}
            decoder = null;
        }
        clearAllVncStripeDecoders();
        cleanupVideoBuffer();
        cleanupJpegStripeQueue();
        clearDecodedStripesQueue();

        if (!isSharedMode) {
            stopMicrophoneCapture();
            if (!isTokenAuthMode) {
                initializeInput();
            }
            if (currentEncoderMode !== 'jpeg' && currentEncoderMode !== 'h264enc' && currentEncoderMode !== 'openh264enc' && currentEncoderMode !== 'h264enc-striped') {
              initializeDecoder();
            }
        }

        initializeAudio().then(() => {
          initializeDecoderAudio();
        });

        if (isTokenAuthMode) {
            initializeInput();
        }

        if (window.webrtcInput && typeof window.webrtcInput.setTrackpadMode === 'function') {
          window.webrtcInput.setTrackpadMode(trackpadMode);
        }
        if (trackpadMode) {
          if (websocket && websocket.readyState === WebSocket.OPEN) {
            websocket.send("SET_NATIVE_CURSOR_RENDERING,1");
            console.log('[websockets] Applied trackpad mode on initialization.');
          }
        }

        if (playButtonElement) playButtonElement.classList.add('hidden');
        if (statusDisplayElement) statusDisplayElement.classList.remove('hidden');

        requestAnimationFrame(paintVideoFrame);

        if (isSharedMode) {
            sharedClientState = 'ready';
            console.log("Shared mode: Received 'MODE websockets'. Requesting initial stream with STOP/START_VIDEO. State: ready.");
            // Initialize the decoder now so it is configured before the first keyframe arrives.
            triggerInitializeDecoder();
            if (websocket && websocket.readyState === WebSocket.OPEN) {
                 websocket.send('STOP_VIDEO');
                 setTimeout(() => {
                    if (websocket && websocket.readyState === WebSocket.OPEN) {
                        websocket.send('START_VIDEO');
                        console.log("Shared mode: Sent START_VIDEO after initial STOP_VIDEO.");
                    }
                }, 250);
            }
        } else {
            if (websocket && websocket.readyState === WebSocket.OPEN) {
              if (isAudioPipelineActive) websocket.send('START_AUDIO');
            }
        }
        loadingText = 'Waiting for stream...';
        updateStatusDisplay();
        initializationComplete = true;
      }
      else if (clientMode === 'websockets') {
        if (event.data.startsWith('{')) {
          let obj;
          try {
            obj = JSON.parse(event.data);
          } catch (e) {
            console.error('Error parsing JSON:', e);
            return;
          }
          if (obj.type === 'system_stats') window.system_stats = obj;
          else if (obj.type === 'gpu_stats') window.gpu_stats = obj;
          else if (obj.type === 'network_stats') window.network_stats = obj;
          else if (obj.type === 'server_settings') {
              if (displayId !== 'primary' && obj.settings.second_screen && obj.settings.second_screen.value === false) {
                  console.error("Server configuration prohibits secondary displays. This client will not function.");
                  if (statusDisplayElement) {
                      statusDisplayElement.textContent = 'Error: Secondary displays are disabled on the server.';
                      statusDisplayElement.classList.remove('hidden');
                  }
                  if (websocket) {
                      websocket.onclose = () => {};
                      websocket.close();
                  }
                  if (reconnectIntervalId) {
                      clearInterval(reconnectIntervalId);
                      reconnectIntervalId = null;
                  }
                  return;
              }
              const changes = sanitizeAndStoreSettings(obj.settings);
              // Server-applied values also drive the module-level mirrors the ingest and
              // decode paths read. Unlike the dashboard path this persists nothing, so a
              // server default stays re-pushable on the next load.
              if (typeof window['encoder'] === 'string' && window['encoder'] !== currentEncoderMode) {
                  const newEnc = window['encoder'];
                  console.log(`Server settings switch encoder ${currentEncoderMode} -> ${newEnc}.`);
                  currentEncoderMode = newEnc;
                  if (decoder && decoder.state !== 'closed') {
                      decoder.close();
                      decoder = null;
                  }
                  if (newEnc !== 'h264enc-striped') {
                      clearAllVncStripeDecoders();
                  }
                  cleanupVideoBuffer();
                  cleanupJpegStripeQueue();
                  clearDecodedStripesQueue();
              }
              if (Number.isFinite(parseInt(window['framerate'], 10))) {
                  framerate = parseInt(window['framerate'], 10);
              }
              if (typeof window['video_fullcolor'] === 'boolean') {
                  video_fullcolor = window['video_fullcolor'];
              }
              if (typeof window['video_streaming_mode'] === 'boolean') {
                  video_streaming_mode = window['video_streaming_mode'];
              }
              // Gate 'cmd,' sends on the server-advertised value (NOT window.command_enabled,
              // which for an unlocked bool keeps the client's persisted localStorage value).
              // Absent/malformed entry => true, so older servers behave as before.
              const wsMax = obj.settings && obj.settings.ws_max_message_bytes;
              if (wsMax && typeof wsMax.value === 'number') applyWsMessageBudget(wsMax.value);
              const ce = obj.settings && obj.settings.command_enabled;
              serverCommandEnabled = (ce && typeof ce.value === 'boolean') ? ce.value : true;
              // Clipboard direction/binary gates are deployment policy: the server
              // value wins over any persisted client preference.
              const cin = obj.settings && obj.settings.clipboard_in_enabled;
              if (cin && typeof cin.value === 'boolean') clipboard_in_enabled = cin.value;
              const cout = obj.settings && obj.settings.clipboard_out_enabled;
              if (cout && typeof cout.value === 'boolean') clipboard_out_enabled = cout.value;
              const ebc = obj.settings && obj.settings.enable_binary_clipboard;
              if (ebc && typeof ebc.value === 'boolean') enable_binary_clipboard = ebc.value;
              window.postMessage({ type: 'serverSettings', payload: obj.settings }, window.location.origin);
              if (Object.keys(changes).length > 0) {
                  console.log('Client settings were sanitized by server rules. Sending updates back to server:', changes);
                  handleSettingsMessage(changes);
              }
              const serverForcesManual = obj.settings && obj.settings.is_manual_resolution_mode && obj.settings.is_manual_resolution_mode.value === true;

              if (serverForcesManual || window.is_manual_resolution_mode) {
                  console.log(`Manual resolution mode active (Server forced: ${serverForcesManual}, Client pref: ${window.is_manual_resolution_mode}). Switching to manual resize handlers.`);
                  if (serverForcesManual) {
                      const serverWidth = obj.settings.manual_width ? parseInt(obj.settings.manual_width.value, 10) : 0;
                      const serverHeight = obj.settings.manual_height ? parseInt(obj.settings.manual_height.value, 10) : 0;
                      if (serverWidth > 0 && serverHeight > 0) {
                          console.log(`Applying server-enforced manual resolution: ${serverWidth}x${serverHeight}`);
                          window.is_manual_resolution_mode = true;
                          manual_width = serverWidth;
                          manual_height = serverHeight;
                          applyManualCanvasStyle(manual_width, manual_height, scaleLocallyManual);
                      } else {
                          console.warn("Server dictated manual mode but did not provide valid dimensions.");
                      }
                  } else {
                      if (manual_width && manual_height) {
                          applyManualCanvasStyle(manual_width, manual_height, scaleLocallyManual);
                      }
                  }
                  disableAutoResize();
              } else {
                  console.log("Server settings payload confirms auto mode. Switching to auto resize handlers.");
                  enableAutoResize();
              }
          }
          else if (obj.type === 'server_apps') {
            if (obj.apps && Array.isArray(obj.apps)) {
              window.postMessage({
                type: 'systemApps',
                apps: obj.apps
              }, window.location.origin);
            }
          } else if (obj.type === 'pipeline_status') {
            let statusChanged = false;
            if (obj.video !== undefined && obj.video !== isVideoPipelineActive) {
              isVideoPipelineActive = obj.video;
              statusChanged = true;
              if (!isVideoPipelineActive && (currentEncoderMode === 'h264enc' || currentEncoderMode === 'openh264enc' || currentEncoderMode === 'h264enc-striped') && !isSharedMode) {
                  clearAllVncStripeDecoders();
              }
            }
            if (obj.audio !== undefined && obj.audio !== isAudioPipelineActive) {
              isAudioPipelineActive = obj.audio;
              statusChanged = true;
              if (audioDecoderWorker) audioDecoderWorker.postMessage({
                type: 'updatePipelineStatus',
                data: {
                  isActive: isAudioPipelineActive
                }
              });
            }
            if (statusChanged) window.postMessage({
              type: 'pipelineStatusUpdate',
              video: isVideoPipelineActive,
              audio: isAudioPipelineActive
            }, window.location.origin);
         } else if (obj.type === 'stream_resolution') {
           if (isSharedMode) {
             if (sharedClientState === 'error' || sharedClientState === 'idle') {
               console.log(`Shared mode: Received stream_resolution while in state '${sharedClientState}'. Ignoring.`);
             } else {
               const physicalNewWidth = parseInt(obj.width, 10);
               const physicalNewHeight = parseInt(obj.height, 10);

               if (physicalNewWidth > 0 && physicalNewHeight > 0) {
                 // Shared-mode canvas sizing works in physical stream pixels
                 // (applyManualCanvasStyle and handleDecodedFrame both use dpr=1
                 // in shared mode); the viewer's own devicePixelRatio is
                 // unrelated to the primary client's stream dimensions.
                 const alignedNewWidth = alignResolution(physicalNewWidth);
                 const alignedNewHeight = alignResolution(physicalNewHeight);
                 let dimensionsChanged = (manual_width !== alignedNewWidth || manual_height !== alignedNewHeight);

                 if (dimensionsChanged) {
                   console.log(`Shared mode: Received new stream resolution ${alignedNewWidth}x${alignedNewHeight} (physical).`);
                   manual_width = alignedNewWidth;
                   manual_height = alignedNewHeight;
                   applyManualCanvasStyle(manual_width, manual_height, true);
                 }

                 if (sharedClientState === 'ready' && dimensionsChanged) {
                   console.log(`Shared mode: Triggering main decoder re-init and clearing canvas for new resolution.`);
                   triggerInitializeDecoder();
                   if (canvasContext && canvas.width > 0 && canvas.height > 0) {
                     canvasContext.setTransform(1, 0, 0, 1, 0, 0);
                     canvasContext.clearRect(0, 0, canvas.width, canvas.height);
                   }
                 }
               } else {
                 console.warn(`Shared mode: Received invalid stream_resolution dimensions: ${obj.width}x${obj.height}`);
               }
             }
           }
         } else {
            console.warn(`Unexpected JSON message type:`, obj.type, obj);
          }
        } else if (event.data.startsWith('cursor,')) {
          try {
            const cursorData = JSON.parse(event.data.substring(7));
            if (window.webrtcInput && typeof window.webrtcInput.updateServerCursor === 'function') {
                window.webrtcInput.updateServerCursor(cursorData);
            }
          } catch (e) {
            console.error('Error parsing cursor data:', e);
          }
        } else if (event.data.startsWith('clipboard_start,')) {
            const parts = event.data.split(',');
            multipartClipboard.mimeType = parts[1];
            multipartClipboard.totalSize = parseInt(parts[2], 10);
            multipartClipboard.receivedSize = 0;
            multipartClipboard.data = [];
            multipartClipboard.inProgress = true;
            console.log(`Starting multi-part clipboard download: ${multipartClipboard.mimeType}, total size: ${multipartClipboard.totalSize}`);
        } else if (event.data.startsWith('clipboard_data,')) {
            if (multipartClipboard.inProgress) {
                try {
                    const base64Chunk = event.data.substring(15);
                    const binaryString = atob(base64Chunk);
                    const len = binaryString.length;
                    const bytes = new Uint8Array(len);
                    for (let i = 0; i < len; i++) {
                        bytes[i] = binaryString.charCodeAt(i);
                    }
                    multipartClipboard.data.push(bytes);
                    multipartClipboard.receivedSize += bytes.byteLength;
                } catch (e) {
                    console.error('Error processing multi-part clipboard chunk:', e);
                    multipartClipboard.inProgress = false;
                }
            }
        } else if (event.data === 'clipboard_finish') {
            if (multipartClipboard.inProgress) {
                console.log(`Finished multi-part clipboard download. Received ${multipartClipboard.receivedSize} of ${multipartClipboard.totalSize} bytes.`);
                if (multipartClipboard.receivedSize !== multipartClipboard.totalSize) {
                    console.error('Multipart clipboard size mismatch. Aborting.');
                } else {
                    try {
                        const blob = new Blob(multipartClipboard.data, { type: multipartClipboard.mimeType });
                        if (multipartClipboard.mimeType === 'text/plain') {
                            blob.text().then(text => {
                                lastServerClipboardText = text;
                                lastServerClipboardMime = 'text/plain';
                                // Settle any pending Ctrl/Cmd+C copy promise.
                                resolveServerClipboard(text, null, 'text/plain');
                                // Local write is gated per-direction (server->client = out).
                                if (clipboard_out_enabled) {
                                    navigator.clipboard.writeText(text).catch(err => console.error('Could not copy server clipboard text to local: ' + err));
                                }
                                window.postMessage({ type: 'clipboardContentUpdate', text: text }, window.location.origin);
                            });
                        } else if (clipboard_out_enabled) {
                            // Settle any pending Ctrl/Cmd+C copy promise with the image blob.
                            resolveServerClipboard(undefined, blob, multipartClipboard.mimeType);
                            const clipboardItem = new ClipboardItem({ [multipartClipboard.mimeType]: blob });
                            navigator.clipboard.write([clipboardItem]).then(() => {
                                console.log(`Successfully wrote multi-part image (${multipartClipboard.mimeType}) from server to local clipboard.`);
                                const uiText = `Image (${multipartClipboard.mimeType}) received from session and copied to clipboard.`;
                                window.postMessage({ type: 'clipboardContentUpdate', text: uiText }, window.location.origin);
                            }).catch(err => {
                                console.error('Failed to write multi-part image to clipboard:', err);
                            });
                        }
                    } catch (e) {
                        console.error('Error assembling final clipboard content:', e);
                    }
                }
                multipartClipboard.inProgress = false;
                multipartClipboard.data = [];
            }
        } else if (event.data.startsWith('clipboard_binary,')) {
            if (!enable_binary_clipboard) {
                console.warn("Received binary clipboard data from server, but feature is disabled on client. Ignoring.");
                return;
            }
            if (!clipboard_out_enabled) {
                console.warn("Received server clipboard image while server->client sync is disabled. Ignoring.");
                return;
            }
            try {
                const parts = event.data.split(',');
                if (parts.length < 3) {
                    console.error('Malformed binary clipboard message from server:', event.data);
                    return;
                }
                const mimeType = parts[1];
                const base64Data = parts[2];
                const binaryString = atob(base64Data);
                const len = binaryString.length;
                const bytes = new Uint8Array(len);
                for (let i = 0; i < len; i++) {
                    bytes[i] = binaryString.charCodeAt(i);
                }
                const blob = new Blob([bytes], { type: mimeType });
                // Settle any pending Ctrl/Cmd+C copy promise with this fresh
                // image blob (binary requests resolve to the Blob, text to its text()).
                resolveServerClipboard(undefined, blob, mimeType);
                const clipboardItem = new ClipboardItem({ [mimeType]: blob });
                navigator.clipboard.write([clipboardItem]).then(() => {
                    console.log(`Successfully wrote image (${mimeType}) from server to local clipboard.`);
                    const uiText = `Image (${mimeType}) received from session and copied to clipboard.`;
                    window.postMessage({ type: 'clipboardContentUpdate', text: uiText }, window.location.origin);
                }).catch(err => {
                    console.error('Failed to write image to clipboard:', err);
                });
            } catch (e) {
                console.error('Error processing binary clipboard data from server:', e);
            }
        } else if (event.data.startsWith('clipboard,')) {
          try {
            const base64Payload = event.data.substring(10);
            const binaryString = atob(base64Payload);
            const len = binaryString.length;
            const bytes = new Uint8Array(len);
            for (let i = 0; i < len; i++) {
                bytes[i] = binaryString.charCodeAt(i);
            }
            const decodedText = new TextDecoder().decode(bytes);
            lastServerClipboardText = decodedText;
            lastServerClipboardMime = 'text/plain';
            // Settle any pending Ctrl/Cmd+C copy promise with this fresh
            // text (resolves the ClipboardItem created in the keydown handler).
            resolveServerClipboard(decodedText, null, 'text/plain');
            // Local write is gated per-direction (server->client = out).
            if (clipboard_out_enabled) {
                navigator.clipboard.writeText(decodedText).catch(err => console.error('Could not copy server clipboard to local: ' + err));
            }
            window.postMessage({
              type: 'clipboardContentUpdate',
              text: decodedText
            }, window.location.origin);

          } catch (e) {
            console.error('Error processing clipboard data:', e);
          }
        } else if (event.data.startsWith('system,')) {
          try {
            const systemMsg = JSON.parse(event.data.substring(7));
            if (systemMsg.action === 'reload') window.location.reload();
          } catch (e) {
            console.error('Error parsing system data:', e);
          }
        } else if (event.data === 'VIDEO_STARTED' && !isSharedMode) {
          clearStartVideoWatchdog();
          isVideoPipelineActive = true;
          window.postMessage({ type: 'pipelineStatusUpdate', video: true }, window.location.origin);
        }
        else if (event.data === 'VIDEO_STOPPED' && !isSharedMode) {
          console.log("Client: Received VIDEO_STOPPED. Updating isVideoPipelineActive=false. Expecting PIPELINE_RESETTING from server for full state reset.");
          isVideoPipelineActive = false;
          window.postMessage({ type: 'pipelineStatusUpdate', video: false }, window.location.origin);
        }
        else if (event.data.startsWith('PIPELINE_RESETTING ')) {
            const parts = event.data.split(' ');
            const resetDisplayId = parts.length > 1 ? parts[1] : 'primary';
            console.log(`[websockets] Received PIPELINE_RESETTING for display '${resetDisplayId}'.`);
            if ((isSharedMode && resetDisplayId === 'primary') || (!isSharedMode && resetDisplayId === displayId)) {
                performServerInitiatedVideoReset(`PIPELINE_RESETTING from server for display '${resetDisplayId}'`);

                if (isSharedMode) {
                    console.log(`Shared mode: Primary pipeline reset. Client remains in ready state.`);
                    sharedClientState = 'ready';
                } else {
                    console.log(`Display '${displayId}': Video reset complete.`);
                }
            } else {
                console.log(`Ignoring PIPELINE_RESETTING for '${resetDisplayId}' as this client is '${isSharedMode ? 'shared' : displayId}'.`);
            }
        }
        else if (event.data.startsWith('DISPLAY_CONFIG_UPDATE,')) {
            try {
                const jsonPayload = event.data.substring(event.data.indexOf(',') + 1);
                const payload = JSON.parse(jsonPayload);

                if (displayId === 'primary') {
                    const secondaryConnected = payload.displays.includes('display2');
                    if (isSecondaryDisplayConnected !== secondaryConnected) {
                        console.log(`Secondary display connection status changed to: ${secondaryConnected}`);
                        isSecondaryDisplayConnected = secondaryConnected;
                        applyEffectiveCursorSetting();
                    }
                }
            } catch (e) {
                console.error('Error parsing DISPLAY_CONFIG_UPDATE:', e, 'Original data:', event.data);
            }
        }
        else if (event.data === 'AUDIO_STARTED' && !isSharedMode) {
          isAudioPipelineActive = true;
          window.postMessage({ type: 'pipelineStatusUpdate', audio: true }, window.location.origin);
          if (audioDecoderWorker) audioDecoderWorker.postMessage({ type: 'updatePipelineStatus', data: { isActive: true } });
        } else if (event.data === 'AUDIO_STOPPED' && !isSharedMode) {
          isAudioPipelineActive = false;
          window.postMessage({ type: 'pipelineStatusUpdate', audio: false }, window.location.origin);
          if (audioDecoderWorker) audioDecoderWorker.postMessage({ type: 'updatePipelineStatus', data: { isActive: false } });
        } else if (event.data === 'AUDIO_DISABLED' && !isSharedMode) {
          console.log("Server reports audio is disabled. Tearing down audio workers.");
          audioEnabled = false;
          isAudioPipelineActive = false;
          if (audioDecoderWorker) {
            audioDecoderWorker.postMessage({ type: 'updatePipelineStatus', data: { isActive: false } });
            audioDecoderWorker.postMessage({ type: 'close' });
            setTimeout(() => {
              if (audioDecoderWorker) {
                audioDecoderWorker.terminate();
                audioDecoderWorker = null;
              }
            }, 50);
          }
          if (audioContext) {
            try { audioContext.close(); } catch (e) { console.error("Error closing AudioContext on AUDIO_DISABLED:", e); }
            audioContext = null;
            audioWorkletNode = null;
            audioWorkletProcessorPort = null;
          }
          window.postMessage({ type: 'pipelineStatusUpdate', audio: false }, window.location.origin);
        } else if (event.data === 'MICROPHONE_DISABLED' && !isSharedMode) {
          console.log("Server reports microphone is disabled. Stopping microphone capture.");
          microphoneEnabled = false;
          stopMicrophoneCapture();
          window.postMessage({ type: 'pipelineStatusUpdate', microphone: false }, window.location.origin);
        } else {
          if (window.webrtcInput && window.webrtcInput.on_message && !isSharedMode) {
            window.webrtcInput.on_message(event.data);
          }
        }
      }
    }
  };

  websocket.onmessage = (event) => {
    const d = event.data;
    if (d instanceof ArrayBuffer) {
      if (d.byteLength >= 1 && new Uint8Array(d, 0, 1)[0] === 0x05) {
        // gzip-wrapped control text: inflate (async), preserving control order.
        __wsGzPending++;
        const gz = d.slice(1);
        __wsCtrlChain = __wsCtrlChain.then(async () => {
          try { __rawWsMessage({ data: await __inflateGz(gz) }); }
          catch (e) { console.error('[websockets] gzip control inflate failed:', e); }
          finally { __wsGzPending--; }
        });
        return;
      }
      // Media frame: dispatch immediately (keeps the video/audio hot path sync).
      __rawWsMessage(event);
      return;
    }
    if (d === '_gz,1') {
      // Server can inflate: start gzip'ing our large client->server text sends.
      if (typeof CompressionStream !== 'undefined') wsGzTx = true;
      return;
    }
    // Control text: only defer behind a pending inflation, else dispatch synchronously
    // so ordering vs media (e.g. PIPELINE_RESETTING) is unchanged in the common case.
    if (__wsGzPending > 0) {
      __wsCtrlChain = __wsCtrlChain.then(() => __rawWsMessage({ data: d }));
    } else {
      __rawWsMessage({ data: d });
    }
  };

  websocket.onerror = (event) => {
    console.error('[websockets] Error:', event);
    status = 'error';
    loadingText = 'WebSocket connection error.';
    updateStatusDisplay();
    if (metricsIntervalId) {
      clearInterval(metricsIntervalId);
      metricsIntervalId = null;
    }
    if (backpressureIntervalId) {
      clearInterval(backpressureIntervalId);
      backpressureIntervalId = null;
    }
    releaseWakeLock();
    if (isSharedMode) {
        console.error("Shared mode: WebSocket error. Resetting shared state to 'error'.");
        sharedClientState = 'error';
    }
  };

  websocket.onclose = (event) => {
    console.log('[websockets] Connection closed', event);
    if (event.code === 4001) {
        console.error("Server rejected connection: Invalid token. Disabling reconnect.");
        if (reconnectIntervalId) clearInterval(reconnectIntervalId);
        reconnectIntervalId = null;
        loadingText = 'Connection Failed: Invalid Token';
        updateStatusDisplay();
        return;
    } else if (event.code === 4002) {
        console.log("Server closed connection due to permission change. Reconnecting...");
    }
    status = 'disconnected';
    loadingText = 'WebSocket disconnected. Attempting to reconnect...';
    updateStatusDisplay();
    if (metricsIntervalId) {
      clearInterval(metricsIntervalId);
      metricsIntervalId = null;
    }
    if (backpressureIntervalId) {
      clearInterval(backpressureIntervalId);
      backpressureIntervalId = null;
    }
    releaseWakeLock();
    cleanupVideoBuffer();
    cleanupJpegStripeQueue();
    if (decoder && decoder.state !== "closed") decoder.close();
    clearAllVncStripeDecoders();
    decoder = null;
    if (audioDecoderWorker) {
      audioDecoderWorker.postMessage({
        type: 'close'
      });
      audioDecoderWorker = null;
    }
    if (!isSharedMode) stopMicrophoneCapture();
    isVideoPipelineActive = false;
    isAudioPipelineActive = false;
    isMicrophoneActive = false;
    window.postMessage({
      type: 'pipelineStatusUpdate',
      video: false,
      audio: false
    }, window.location.origin);
    if (isSharedMode) {
        console.log("Shared mode: WebSocket closed. Resetting shared state to 'idle'.");
        sharedClientState = 'idle';
    }
    if (!reconnectIntervalId) {
      reconnectIntervalId = setInterval(() => {
        if (websocket && (websocket.readyState === WebSocket.OPEN || websocket.readyState === WebSocket.CONNECTING)) {
          // Pass
        } else {
          console.log("WebSocket disconnected, reloading page to reconnect.");
          location.reload();
        }
      }, 5000);
    }
  };
}

if (document.readyState === 'loading') {
  document.addEventListener('DOMContentLoaded', initWebsockets);
} else {
  initWebsockets();
}

function cleanupVideoBuffer() {
  let closedCount = 0;
  while (videoFrameBuffer.length > 0) {
    const frame = videoFrameBuffer.shift();
    try {
      frame.close();
      closedCount++;
    } catch (e) {
      /* ignore */
    }
  }
  if (closedCount > 0) console.log(`Cleanup: Closed ${closedCount} video frames from main buffer.`);
  deactivateMstg();
  deactivateVideoWorker();
}

function cleanupJpegStripeQueue() {
  let closedCount = 0;
  while (jpegStripeRenderQueue.length > 0) {
    const segment = jpegStripeRenderQueue.shift();
    if (segment && segment.image && typeof segment.image.close === 'function') {
      try {
        segment.image.close();
        closedCount++;
      } catch (e) {
        /* ignore */
      }
    }
  }
  if (closedCount > 0) console.log(`Cleanup: Closed ${closedCount} JPEG stripe images.`);
  lastDrawnJpegStripeFrameId = {};
}

function clearDecodedStripesQueue() {
  while (decodedStripesQueue.length > 0) {
    const stripeData = decodedStripesQueue.shift();
    try {
      if (stripeData && stripeData.frame) stripeData.frame.close();
    } catch (e) {
      /* ignore */
    }
  }
}

// Surround (>2ch) is Chromium's multistream Opus: the decoder needs an OpusHead
// description carrying the same layout tables the server encodes with.
const MULTIOPUS_CLIENT_LAYOUTS = {
  6: { streams: 4, coupled: 2, mapping: [0, 4, 1, 2, 3, 5] },
  8: { streams: 5, coupled: 3, mapping: [0, 6, 1, 2, 3, 4, 5, 7] },
};

function getAudioChannelCount() {
  const ch = parseInt(window.audio_channels, 10);
  return (ch === 1 || ch === 2 || ch === 6 || ch === 8) ? ch : 2;
}

function buildMultiopusDescription(channels) {
  const layout = MULTIOPUS_CLIENT_LAYOUTS[channels];
  if (!layout) return null;
  const buf = new ArrayBuffer(21 + channels);
  const u8 = new Uint8Array(buf);
  const dv = new DataView(buf);
  u8.set([0x4f, 0x70, 0x75, 0x73, 0x48, 0x65, 0x61, 0x64]); // "OpusHead"
  u8[8] = 1;                    // version
  u8[9] = channels;
  dv.setUint16(10, 0, true);    // pre-skip: live stream, nothing to trim
  dv.setUint32(12, 48000, true);
  dv.setInt16(16, 0, true);     // output gain
  u8[18] = 1;                   // mapping family 1 (multistream)
  u8[19] = layout.streams;
  u8[20] = layout.coupled;
  u8.set(layout.mapping, 21);
  return buf;
}

const audioDecoderWorkerCode = `
  let decoderAudio;
  let pipelineActive = true;
  let currentDecodeQueueSize = 0;
  const decoderConfig = {
    codec: 'opus',
    numberOfChannels: 2,
    sampleRate: 48000,
  };

  async function initializeDecoderInWorker() {
    if (decoderAudio && decoderAudio.state !== 'closed') {
      try { decoderAudio.close(); } catch (e) { /* ignore */ }
    }
    currentDecodeQueueSize = 0;
    decoderAudio = new AudioDecoder({
      output: handleDecodedAudioFrameInWorker,
      error: (e) => {
        console.error('[AudioWorker] AudioDecoder error:', e.message, e);
        currentDecodeQueueSize = Math.max(0, currentDecodeQueueSize -1);
        if (e.message.includes('fatal') || (decoderAudio && (decoderAudio.state === 'closed' || decoderAudio.state === 'unconfigured'))) {
          // initializeDecoderInWorker(); // Avoid rapid re-init loops on persistent errors
        }
      },
    });
    try {
      const support = await AudioDecoder.isConfigSupported(decoderConfig);
      if (support.supported) {
        await decoderAudio.configure(decoderConfig);
        self.postMessage({ type: 'decoderInitialized' });
      } else {
        decoderAudio = null;
        self.postMessage({ type: 'decoderInitFailed', reason: 'configNotSupported' });
      }
    } catch (e) {
      decoderAudio = null;
      self.postMessage({ type: 'decoderInitFailed', reason: e.message });
    }
  }

  async function handleDecodedAudioFrameInWorker(frame) {
    currentDecodeQueueSize = Math.max(0, currentDecodeQueueSize - 1);
    if (!frame || typeof frame.copyTo !== 'function' || typeof frame.allocationSize !== 'function' || typeof frame.close !== 'function') {
        if(frame && typeof frame.close === 'function') { try { frame.close(); } catch(e) { /* ignore */ } }
        return;
    }
    let pcmDataArrayBuffer;
    try {
      const requiredByteLength = frame.allocationSize({ planeIndex: 0, format: 'f32' });
      if (requiredByteLength === 0) {
          try { frame.close(); } catch(e) { /* ignore */ }
          return;
      }
      pcmDataArrayBuffer = new ArrayBuffer(requiredByteLength);
      const pcmDataView = new Float32Array(pcmDataArrayBuffer);
      await frame.copyTo(pcmDataView, { planeIndex: 0, format: 'f32' });
      self.postMessage({ type: 'decodedAudioData', pcmBuffer: pcmDataArrayBuffer }, [pcmDataArrayBuffer]);
      pcmDataArrayBuffer = null;
    } catch (error) { /* console.error */ }
    finally {
      if (frame && typeof frame.close === 'function') {
        try { frame.close(); } catch (e) { /* ignore */ }
      }
    }
  }

  self.onmessage = async (event) => {
    const { type, data } = event.data;
    switch (type) {
      case 'init':
        pipelineActive = data.initialPipelineStatus;
        if (data.channels) {
          decoderConfig.numberOfChannels = data.channels;
        }
        if (data.description) {
          decoderConfig.description = data.description;
        }
        await initializeDecoderInWorker();
        break;
      case 'decode':
        if (decoderAudio && decoderAudio.state === 'configured') {
          const chunk = new EncodedAudioChunk({ type: 'key', timestamp: data.timestamp || (performance.now() * 1000), data: data.opusBuffer });
          try {
            if (currentDecodeQueueSize < 20) {
                 decoderAudio.decode(chunk); currentDecodeQueueSize++;
            }
          } catch (e) {
              currentDecodeQueueSize = Math.max(0, currentDecodeQueueSize - 1);
              if (decoderAudio.state === 'closed' || decoderAudio.state === 'unconfigured') await initializeDecoderInWorker();
          }
        } else if (!decoderAudio || (decoderAudio && decoderAudio.state !== 'configuring')) {
          await initializeDecoderInWorker();
        }
        break;
      case 'reinitialize': await initializeDecoderInWorker(); break;
      case 'updatePipelineStatus': pipelineActive = data.isActive; break;
      case 'close':
        if (decoderAudio && decoderAudio.state !== 'closed') { try { decoderAudio.close(); } catch (e) { /* ignore */ } }
        decoderAudio = null; self.close(); break;
      default: break;
    }
  };
`;

const micWorkletProcessorCode = `
class MicWorkletProcessor extends AudioWorkletProcessor {
  constructor() {
    super();
    this.SILENCE_THRESHOLD_CHUNKS = 300;
    this.silentChunkCounter = 0;
    this.isSending = true;
  }
  process(inputs, outputs, parameters) {
    const input = inputs[0];
    if (input && input[0]) {
      const inputChannelData = input[0];
      const int16Array = Int16Array.from(inputChannelData, x => x * 32767);
      const isCurrentChunkSilent = int16Array.every(item => item === 0);
      if (!isCurrentChunkSilent) {
        this.isSending = true;
        this.silentChunkCounter = 0;
      } else {
        this.silentChunkCounter++;
      }
      if (this.silentChunkCounter >= this.SILENCE_THRESHOLD_CHUNKS) {
        this.isSending = false;
      }
      if (this.isSending) {
        this.port.postMessage(int16Array.buffer, [int16Array.buffer]);
      }
    }
    return true;
  }
}
registerProcessor('mic-worklet-processor', MicWorkletProcessor);
`;

async function startMicrophoneCapture() {
  if (isSharedMode) {
    console.log("Shared mode: Microphone capture blocked.");
    isMicrophoneActive = false;
    postSidebarButtonUpdate();
    return;
  }
  if (isMicrophoneActive || !navigator.mediaDevices || !navigator.mediaDevices.getUserMedia) {
    if (!isMicrophoneActive) isMicrophoneActive = false;
    postSidebarButtonUpdate();
    return;
  }
  let constraints;
  try {
    constraints = {
      audio: {
        deviceId: preferredInputDeviceId ? {
          exact: preferredInputDeviceId
        } : undefined,
        sampleRate: 24000,
        channelCount: 1,
        echoCancellation: true,
        noiseSuppression: true,
        autoGainControl: true
      },
      video: false
    };
    micStream = await navigator.mediaDevices.getUserMedia(constraints);
    const audioTracks = micStream.getAudioTracks();
    if (audioTracks.length > 0) {
      const settings = audioTracks[0].getSettings();
      if (!preferredInputDeviceId && settings.deviceId) preferredInputDeviceId = settings.deviceId;
    }
    if (micAudioContext && micAudioContext.state !== 'closed') await micAudioContext.close();
    micAudioContext = new AudioContext({
      sampleRate: 24000
    });
    if (micAudioContext.state === 'suspended') await micAudioContext.resume();
    if (typeof micWorkletProcessorCode === 'undefined' || !micWorkletProcessorCode) throw new Error("micWorkletProcessorCode undefined");
    const micWorkletBlob = new Blob([micWorkletProcessorCode], {
      type: 'application/javascript'
    });
    const micWorkletURL = URL.createObjectURL(micWorkletBlob);
    try {
      await micAudioContext.audioWorklet.addModule(micWorkletURL);
    } finally {
      URL.revokeObjectURL(micWorkletURL);
    }
    micSourceNode = micAudioContext.createMediaStreamSource(micStream);
    micWorkletNode = new AudioWorkletNode(micAudioContext, 'mic-worklet-processor');
    // Encode the mic to Opus in the page (WebCodecs) so only Opus crosses the wire; the
    // server decodes it in pcmflux, symmetric with the server->client audio direction.
    micTimestampUs = 0;
    micEncoder = new AudioEncoder({
      output: (chunk) => {
        if (!(websocket && websocket.readyState === WebSocket.OPEN && isMicrophoneActive)) return;
        const messageBuffer = new ArrayBuffer(1 + chunk.byteLength);
        new Uint8Array(messageBuffer)[0] = 0x02;
        chunk.copyTo(new Uint8Array(messageBuffer, 1));
        try {
          websocket.send(messageBuffer);
        } catch (e) {
          console.error("Error sending mic Opus:", e);
        }
      },
      error: (e) => console.error("Mic AudioEncoder error:", e)
    });
    micEncoder.configure({ codec: 'opus', sampleRate: 24000, numberOfChannels: 1, bitrate: 32000 });
    micWorkletNode.port.onmessage = (event) => {
      const pcm16Buffer = event.data;
      if (!(micEncoder && micEncoder.state === 'configured' && isMicrophoneActive)) return;
      if (!pcm16Buffer || !(pcm16Buffer instanceof ArrayBuffer) || pcm16Buffer.byteLength === 0) return;
      const numFrames = pcm16Buffer.byteLength / 2;   // mono s16
      const audioData = new AudioData({
        format: 's16', sampleRate: 24000, numberOfFrames: numFrames,
        numberOfChannels: 1, timestamp: micTimestampUs, data: pcm16Buffer
      });
      micTimestampUs += Math.round(numFrames * 1e6 / 24000);
      try { micEncoder.encode(audioData); } catch (e) { console.error("Mic encode error:", e); }
      audioData.close();
    };
    micWorkletNode.port.onmessageerror = (event) => console.error("Error from mic worklet:", event);
    micSourceNode.connect(micWorkletNode);
    isMicrophoneActive = true;
    postSidebarButtonUpdate();
  } catch (error) {
    console.error('Failed to start microphone capture:', error);
    alert(`Microphone error: ${error.name} - ${error.message}`);
    stopMicrophoneCapture();
  }
}

function stopMicrophoneCapture() {
  if (!isMicrophoneActive && !micStream && !micAudioContext) {
    if (isMicrophoneActive) {
      isMicrophoneActive = false;
      postSidebarButtonUpdate();
    }
    return;
  }
  if (micStream) {
    micStream.getTracks().forEach(track => track.stop());
    micStream = null;
  }
  if (micWorkletNode) {
    micWorkletNode.port.onmessage = null;
    micWorkletNode.port.onmessageerror = null;
    try {
      micWorkletNode.disconnect();
    } catch (e) {}
    micWorkletNode = null;
  }
  if (micEncoder) {
    try { if (micEncoder.state !== 'closed') micEncoder.close(); } catch (e) {}
    micEncoder = null;
  }
  if (micSourceNode) {
    try {
      micSourceNode.disconnect();
    } catch (e) {}
    micSourceNode = null;
  }
  if (micAudioContext) {
    if (micAudioContext.state !== 'closed') {
      micAudioContext.close().catch(e => console.error('Error closing mic AudioContext:', e)).finally(() => micAudioContext = null);
    } else {
      micAudioContext = null;
    }
  }
  if (isMicrophoneActive) {
    isMicrophoneActive = false;
    postSidebarButtonUpdate();
  }
}

function cleanup() {
  if (metricsIntervalId) {
    clearInterval(metricsIntervalId);
    metricsIntervalId = null;
  }
  if (backpressureIntervalId) {
    clearInterval(backpressureIntervalId);
    backpressureIntervalId = null;
  }
  releaseWakeLock();
  if (window.isCleaningUp) return;
  window.isCleaningUp = true;
  console.log("Cleanup: Starting cleanup process...");
  if (!isSharedMode) stopMicrophoneCapture();

  if (websocket) {
    websocket.onopen = null;
    websocket.onmessage = null;
    websocket.onerror = null;
    websocket.onclose = null;
    if (websocket.readyState === WebSocket.OPEN || websocket.readyState === WebSocket.CONNECTING) websocket.close();
    websocket = null;
  }
  if (audioContext) {
    if (audioContext.state !== 'closed') audioContext.close().catch(e => console.error('Cleanup error:', e));
    audioContext = null;
    audioWorkletNode = null;
    audioWorkletProcessorPort = null;
    window.currentAudioBufferSize = 0;
    if (audioDecoderWorker) {
      audioDecoderWorker.postMessage({ type: 'close' });
      audioDecoderWorker.terminate(); 
      audioDecoderWorker = null;
    }
  }
  if (decoder && decoder.state !== "closed") {
    decoder.close();
    decoder = null;
  }
  cleanupVideoBuffer();
  cleanupJpegStripeQueue();
  clearAllVncStripeDecoders();
  preferredInputDeviceId = null;
  preferredOutputDeviceId = null;
  status = 'connecting';
  loadingText = '';
  showStart = true;
  streamStarted = false;
  inputInitialized = false;
  if (statusDisplayElement) statusDisplayElement.textContent = 'Connecting...';
  if (statusDisplayElement) statusDisplayElement.classList.remove('hidden');
  if (playButtonElement) playButtonElement.classList.remove('hidden');
  if (overlayInput) overlayInput.style.cursor = 'auto';
  isVideoPipelineActive = true;
  isAudioPipelineActive = true;
  isMicrophoneActive = false;
  window.fps = 0;
  frameCount = 0;
  lastFpsUpdateTime = performance.now();
  console.log("Cleanup: Finished cleanup process.");
  window.isCleaningUp = false;
}

function handleDragOver(ev) {
  if (isSharedMode) {
      ev.preventDefault();
      ev.dataTransfer.dropEffect = 'none';
      return;
  }
  ev.preventDefault();
  ev.dataTransfer.dropEffect = 'copy';
}

async function handleDrop(ev) {
  ev.preventDefault();
  ev.stopPropagation();
  if (isSharedMode) {
    console.log("Shared mode: File upload via drag-drop blocked.");
    return;
  }
  if (!websocket || websocket.readyState !== WebSocket.OPEN) {
    window.postMessage({
      type: 'fileUpload',
      payload: {
        status: 'error',
        fileName: 'N/A',
        message: "WebSocket not open."
      }
    }, window.location.origin);
    return;
  }
  const entriesToProcess = [];
  if (ev.dataTransfer.items) {
    for (let i = 0; i < ev.dataTransfer.items.length; i++) {
      const entry = ev.dataTransfer.items[i].webkitGetAsEntry() || ev.dataTransfer.items[i].getAsEntry();
      if (entry) entriesToProcess.push(entry);
    }
  } else if (ev.dataTransfer.files.length > 0) {
    for (let i = 0; i < ev.dataTransfer.files.length; i++) {
      await uploadFileObject(ev.dataTransfer.files[i], ev.dataTransfer.files[i].name);
    }
    return;
  }

  try {
    for (const entry of entriesToProcess) await handleDroppedEntry(entry);
  } catch (error) {
    const errorMsg = `Error during sequential upload: ${error.message || error}`;
    window.postMessage({
      type: 'fileUpload',
      payload: {
        status: 'error',
        fileName: 'N/A',
        message: errorMsg
      }
    }, window.location.origin);
    if (websocket && websocket.readyState === WebSocket.OPEN) websocket.send(`FILE_UPLOAD_ERROR:${b64Path('GENERAL')}:Processing failed`);
  }
}

function getFileFromEntry(fileEntry) {
  return new Promise((resolve, reject) => fileEntry.file(resolve, reject));
}

async function handleDroppedEntry(entry, basePathFallback = "") {
  let pathToSend;
  if (entry.fullPath && typeof entry.fullPath === 'string' && entry.fullPath !== entry.name && (entry.fullPath.includes('/') || entry.fullPath.includes('\\'))) {
    pathToSend = entry.fullPath;
    if (pathToSend.startsWith('/')) {
        pathToSend = pathToSend.substring(1);
    }
    console.log(`Using entry.fullPath: "${pathToSend}" for entry.name: "${entry.name}"`);
  } else {
    pathToSend = basePathFallback ? `${basePathFallback}/${entry.name}` : entry.name;
    console.log(`Constructed path: "${pathToSend}" for entry.name: "${entry.name}" (basePathFallback: "${basePathFallback}")`);
  }

  if (entry.isFile) {
    try {
      const file = await getFileFromEntry(entry);
      await uploadFileObject(file, pathToSend);
    } catch (err) {
      console.error(`Error processing file ${pathToSend}: ${err}`);
       window.postMessage({
        type: 'fileUpload',
        payload: { status: 'error', fileName: pathToSend, message: `Error processing file: ${err.message || err}` }
      }, window.location.origin);
      if (websocket && websocket.readyState === WebSocket.OPEN) {
         websocket.send(`FILE_UPLOAD_ERROR:${b64Path(pathToSend)}:Client-side file processing error`);
      }
    }
  } else if (entry.isDirectory) {
    console.log(`Processing directory: ${pathToSend}`);
    const dirReader = entry.createReader();
    let entries;
    do {
      entries = await new Promise((resolve, reject) => dirReader.readEntries(resolve, reject));
      for (const subEntry of entries) {
        await handleDroppedEntry(subEntry, pathToSend);
      }
    } while (entries.length > 0);
  }
}

function readEntriesPromise(dirReader) {
  return new Promise((resolve, reject) => dirReader.readEntries(resolve, reject));
}

async function readDirectoryEntries(dirReader) {
  let entries;
  do {
    entries = await readEntriesPromise(dirReader);
    for (const entry of entries) await handleDroppedEntry(entry);
  } while (entries.length > 0);
}

function uploadFileObject(file, pathToSend) {
  return new Promise((resolve, reject) => {
    if (!websocket || websocket.readyState !== WebSocket.OPEN) {
      const errorMsg = `WS closed for ${pathToSend}.`;
      window.postMessage({
        type: 'fileUpload',
        payload: {
          status: 'error',
          fileName: pathToSend,
          message: errorMsg
        }
      }, window.location.origin);
      reject(new Error(errorMsg));
      return;
    }

    window.postMessage({
      type: 'fileUpload',
      payload: {
        status: 'start',
        fileName: pathToSend,
        fileSize: file.size
      }
    }, window.location.origin);
    
    websocket.send(`FILE_UPLOAD_START:${b64Path(pathToSend)}:${file.size}`);
    
    let offset = 0;
    fileUploadProgressLastSent[pathToSend] = 0;
    
    const MAX_BUFFER_THRESHOLD = 10 * 1024 * 1024;
    const BUFFER_CHECK_INTERVAL_MS = 50; 

    const reader = new FileReader();

    reader.onload = function(e) {
      if (!websocket || websocket.readyState !== WebSocket.OPEN) {
        const uploadErrorMsg = `WS closed during upload of ${pathToSend}`;
        window.postMessage({ type: 'fileUpload', payload: { status: 'error', fileName: pathToSend, message: uploadErrorMsg }}, window.location.origin);
        reject(new Error(uploadErrorMsg));
        return;
      }

      if (e.target.error) {
        const readErrorMsg = `File read error for ${pathToSend}: ${e.target.error}`;
        window.postMessage({ type: 'fileUpload', payload: { status: 'error', fileName: pathToSend, message: readErrorMsg }}, window.location.origin);
        websocket.send(`FILE_UPLOAD_ERROR:${b64Path(pathToSend)}:File read error`);
        reject(e.target.error);
        return;
      }

      try {
        const resultLen = e.target.result.byteLength;
        const prefixedView = new Uint8Array(1 + resultLen);
        prefixedView[0] = 0x01;
        prefixedView.set(new Uint8Array(e.target.result), 1);
        websocket.send(prefixedView.buffer);
        offset += resultLen;
        const progress = file.size > 0 ? Math.round((offset / file.size) * 100) : 100;
        const now = Date.now();
        
        if (now - fileUploadProgressLastSent[pathToSend] > FILE_UPLOAD_THROTTLE_MS) {
          window.postMessage({
            type: 'fileUpload',
            payload: {
              status: 'progress',
              fileName: pathToSend,
              progress: progress,
              fileSize: file.size
            }
          }, window.location.origin);
          fileUploadProgressLastSent[pathToSend] = now;
        }

        if (offset < file.size) {
          attemptNextRead(offset);
        } else {
          window.postMessage({
            type: 'fileUpload',
            payload: { status: 'progress', fileName: pathToSend, progress: 100, fileSize: file.size }
          }, window.location.origin);
          
          websocket.send(`FILE_UPLOAD_END:${b64Path(pathToSend)}`);
          
          window.postMessage({
            type: 'fileUpload',
            payload: {
              status: 'end',
              fileName: pathToSend,
              fileSize: file.size
            }
          }, window.location.origin);
          resolve();
        }

      } catch (wsError) {
        const sendErrorMsg = `WS send error during upload of ${pathToSend}: ${wsError.message || wsError}`;
        window.postMessage({ type: 'fileUpload', payload: { status: 'error', fileName: pathToSend, message: sendErrorMsg }}, window.location.origin);
        websocket.send(`FILE_UPLOAD_ERROR:${b64Path(pathToSend)}:WS send error`);
        reject(wsError);
      }
    };

    reader.onerror = function(e) {
      const generalReadError = `General file reader error for ${pathToSend}: ${e.target.error}`;
      window.postMessage({ type: 'fileUpload', payload: { status: 'error', fileName: pathToSend, message: generalReadError }}, window.location.origin);
      websocket.send(`FILE_UPLOAD_ERROR:${b64Path(pathToSend)}:General file reader error`);
      reject(e.target.error);
    };

    function attemptNextRead(currentOffset) {
      if (websocket.bufferedAmount > MAX_BUFFER_THRESHOLD) {
        setTimeout(() => attemptNextRead(currentOffset), BUFFER_CHECK_INTERVAL_MS);
      } else {
        readChunk(currentOffset);
      }
    }

    function readChunk(startOffset) {
      if (!websocket || websocket.readyState !== WebSocket.OPEN) {
        const chunkReadError = `WS closed before reading next chunk of ${pathToSend}`;
        window.postMessage({ type: 'fileUpload', payload: { status: 'error', fileName: pathToSend, message: chunkReadError }}, window.location.origin);
        reject(new Error(chunkReadError));
        return;
      }
      const slice = file.slice(startOffset, Math.min(startOffset + UPLOAD_CHUNK_SIZE, file.size));
      reader.readAsArrayBuffer(slice);
    }
    readChunk(0);
  });
}

function performServerInitiatedVideoReset(reason = "unknown") {
  console.log(`Performing server-initiated video reset. Reason: ${reason}. Current lastReceivedVideoFrameId before reset: ${lastReceivedVideoFrameId}`);

  if (isSharedMode) {
    sharedClientHasReceivedKeyframe = false;
    pendingSharedKeyframe = null;
    console.log("  Shared mode reset: Gate closed. Waiting for a new keyframe.");
  }

  lastReceivedVideoFrameId = -1;
  console.log(`  Reset lastReceivedVideoFrameId to ${lastReceivedVideoFrameId}.`);

  cleanupVideoBuffer();
  cleanupJpegStripeQueue();
  clearDecodedStripesQueue();

  if (currentEncoderMode === 'h264enc' || currentEncoderMode === 'openh264enc' || currentEncoderMode === 'h264enc-striped') {
    clearAllVncStripeDecoders();
  } else if (currentEncoderMode !== 'jpeg') {
    if (decoder && decoder.state !== 'closed') {
      console.log("  Closing main video decoder due to server reset.");
      try { decoder.close(); } catch(e) { console.warn("  Error closing main video decoder during reset:", e); }
    }
    decoder = null;
    console.log("  Main video decoder instance set to null.");
  }

  if (canvasContext && canvas && !(currentEncoderMode === 'h264enc' || currentEncoderMode === 'openh264enc' || currentEncoderMode === 'h264enc-striped')) {
    try {
      canvasContext.setTransform(1, 0, 0, 1, 0, 0);
      canvasContext.clearRect(0, 0, canvas.width, canvas.height);
      console.log("  Cleared canvas during server-initiated reset.");
    } catch (e) {
      console.error("  Error clearing canvas during server-initiated reset:", e);
    }
  }

  if (!isSharedMode) {
    if (currentEncoderMode !== 'jpeg' && currentEncoderMode !== 'h264enc' && currentEncoderMode !== 'openh264enc' && currentEncoderMode !== 'h264enc-striped') {
      console.log("  Ensuring main video decoder is re-initialized after server reset.");
      if (isVideoPipelineActive) {
         triggerInitializeDecoder();
      } else {
        console.log("  isVideoPipelineActive is false, decoder re-initialization deferred until video is enabled by user.");
      }
    }
  }
}

let lastKeyframeRequestTime = 0;
// Ask the server (pixelflux) for an IDR when a decoder is waiting for its first
// keyframe (e.g. after a stripe decoder is recreated, or a shared viewer's keyframe
// gate is closed). The GOP is infinite by default, so this is the only recovery path —
// shared viewers must request too. Debounced (harder for shared); server rate-limits.
function requestKeyframe() {
    const now = performance.now();
    if (now - lastKeyframeRequestTime < (isSharedMode ? 1500 : 500)) return;
    lastKeyframeRequestTime = now;
    if (websocket && websocket.readyState === WebSocket.OPEN) {
        websocket.send("REQUEST_KEYFRAME");
    }
}

function initiateFallback(error, context) {
    if (error.name === 'QuotaExceededError' || (error.message && error.message.includes('reclaimed'))) {
        console.warn(`[initiateFallback] Ignoring soft error (Context: ${context}): Codec reclaimed by browser. Waiting for tab focus to re-initialize.`);
        return; 
    }
    console.error(`FATAL DECODER ERROR (Context: ${context}).`, error);
    if (window.isFallingBack) return;
    window.isFallingBack = true;
    if (websocket && websocket.readyState === WebSocket.OPEN) {
        websocket.onclose = null;
        websocket.close();
    }
    if (metricsIntervalId) {
      clearInterval(metricsIntervalId);
      metricsIntervalId = null;
    }
    if (isSharedMode) {
        console.log("Shared client fallback: Reloading page to re-sync with the stream.");
        if (statusDisplayElement) {
            statusDisplayElement.textContent = 'A video error occurred. Reloading to re-sync with the stream...';
            statusDisplayElement.classList.remove('hidden');
        }
    } else {
        console.log("Primary client fallback: Forcing client settings to safe defaults.");
        const crashKey = `${storageAppName}_crash_count`;
        let crashCount = parseInt(window.localStorage.getItem(crashKey) || '0');
        crashCount++;
        safeSetItem(crashKey, crashCount.toString());
        if (crashCount >= 3) {
            setStringParam('encoder', 'jpeg');
            safeSetItem(crashKey, '0');
        } else {
            setStringParam('encoder', 'h264enc');
        }
        setBoolParam('video_fullcolor', false);
        setIntParam('framerate', 60);
        setIntParam('video_crf', 25);
        setBoolParam('is_manual_resolution_mode', false);
        setIntParam('manual_width', null);
        setIntParam('manual_height', null);
        
        if (statusDisplayElement) {
            statusDisplayElement.textContent = 'A critical video error occurred. Resetting to default settings and reloading...';
            statusDisplayElement.classList.remove('hidden');
        }
    }
    setTimeout(() => {
        window.location.reload();
    }, 3000);
}

function runPreflightChecks() {
    initializeUI();
    if (!window.isSecureContext) {
        console.error("FATAL: Not in a secure context. WebCodecs require HTTPS.");
        if (statusDisplayElement) {
            statusDisplayElement.textContent = 'Error: This application requires a secure connection (HTTPS). Please check the URL.';
            statusDisplayElement.classList.remove('hidden');
        }
        if (playButtonElement) playButtonElement.classList.add('hidden');
        return false;
    }

    if (typeof window.VideoDecoder === 'undefined') {
        console.error("FATAL: Browser does not support the VideoDecoder API.");
        if (statusDisplayElement) {
            statusDisplayElement.textContent = 'Error: Your browser does not support the WebCodecs API required for video streaming.';
            statusDisplayElement.classList.remove('hidden');
        }
        if (playButtonElement) playButtonElement.classList.add('hidden');
        return false;
    }

    console.log("Pre-flight checks passed: Secure context and VideoDecoder API are available.");
    return true;
}

window.addEventListener('beforeunload', cleanup);
window.webrtcInput = null;
}
