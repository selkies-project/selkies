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
let decoder;
let isSidebarOpen = false;
let audioDecoderWorker = null;
let canvas = null;
let canvasContext = null;
let websocket;
let clientMode = null;
let audioContext;
let audioWorkletNode;
let audioWorkletProcessorPort;
window.currentAudioBufferSize = 0;
let videoFrameBuffer = [];
let jpegStripeRenderQueue = [];
let videoBufferSize = 0;
let triggerInitializeDecoder = () => {
  console.error("initializeDecoder function not yet assigned!");
};
let isVideoPipelineActive = true;
let isAudioPipelineActive = true;
let isMicrophoneActive = false;
let isGamepadEnabled;
let lastReceivedVideoFrameId = -1;
let initializationComplete = false;
// Microphone related resources
let micStream = null;
let micAudioContext = null;
let micSourceNode = null;
let micWorkletNode = null;
let preferredInputDeviceId = null;
let preferredOutputDeviceId = null;
let metricsIntervalId = null;
let backpressureIntervalId = null;
let heartbeatIntervalId = null;
const METRICS_INTERVAL_MS = 500;
const BACKPRESSURE_INTERVAL_MS = 50;
const HEARTBEAT_INTERVAL_MS = 90000; // 90 seconds - less than Cloudflare's 100s timeout
const UPLOAD_CHUNK_SIZE = (1024 * 1024) - 1;
const FILE_UPLOAD_THROTTLE_MS = 200;
let fileUploadProgressLastSent = {};
// Resources for resolution controls
window.isManualResolutionMode = false;
let manualWidth = null;
let manualHeight = null;
let originalWindowResizeHandler = null;
let handleResizeUI_globalRef = null;
let vncStripeDecoders = {};
let wakeLockSentinel = null;
let currentEncoderMode = 'x264enc-stiped';
let useCssScaling = false;
let trackpadMode = false;
let scalingDPI = 96;
let antiAliasingEnabled = true;
let useBrowserCursors = false;
function setRealViewportHeight() {
  const vh = window.innerHeight * 0.01;
  document.documentElement.style.setProperty('--vh', `${vh}px`);
}


let detectedSharedModeType = null;
let playerInputTargetIndex = 0; // Default for primary player

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
}
let sharedClientState = 'idle'; // Possible states: 'idle', 'awaiting_identification', 'configuring', 'ready', 'error'
let identifiedEncoderModeForShared = null; // e.g., 'h264_full_frame', 'jpeg', 'x264enc-striped'
const SHARED_PROBING_TIMEOUT_MS = 7000; // Timeout for waiting for the first video packet
let sharedProbingTimeoutId = null;
let sharedProbingAttempts = 0;
const MAX_SHARED_PROBING_ATTEMPTS = 3; // e.g., initial + 2 retries
const isSharedMode = detectedSharedModeType !== null;
let sharedClientHasReceivedKeyframe = false;

if (isSharedMode) {
  console.log(`Client is running in ${detectedSharedModeType} mode.`);
}

window.onload = () => {
  'use strict';
};

// Set storage key based on URL
const urlForKey = window.location.href.split('#')[0];
const storageAppName = urlForKey.replace(/[^a-zA-Z0-9.-_]/g, '_');

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

let videoBitRate = 8000;
let videoFramerate = 60;
let videoCRF = 25;
let h264_fullcolor = false;
let h264_streaming_mode = false;
let jpegQuality = 60;
let paintOverJpegQuality = 90;
let useCpu = false;
let h264_paintover_crf = 18;
let h264_paintover_burst_frames = 5;
let use_paint_over_quality = true;
let audioBitRate = 320000;
let showStart = true;
let status = 'connecting';
let loadingText = '';
const gamepad = {
  gamepadState: 'disconnected',
  gamepadName: 'none',
};
const connectionStat = {
  connectionStatType: 'unknown',
  connectionLatency: 0,
  connectionVideoLatency: 0,
  connectionAudioLatency: 0,
  connectionAudioCodecName: 'NA',
  connectionAudioBitrate: 0,
  connectionPacketsReceived: 0,
  connectionPacketsLost: 0,
  connectionBytesReceived: 0,
  connectionBytesSent: 0,
  connectionCodec: 'unknown',
  connectionVideoDecoder: 'unknown',
  connectionResolution: '',
  connectionFrameRate: 0,
  connectionVideoBitrate: 0,
  connectionAvailableBandwidth: 0,
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
let resizeRemote = true;
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
let videoElement;
let audioElement;
let playButtonElement;
let overlayInput;

const getIntParam = (key, default_value) => {
  const prefixedKey = `${storageAppName}_${key}`;
  const value = window.localStorage.getItem(prefixedKey);
  return (value === null || value === undefined) ? default_value : parseInt(value);
};
const setIntParam = (key, value) => {
  const prefixedKey = `${storageAppName}_${key}`;
  if (value === null || value === undefined) {
    window.localStorage.removeItem(prefixedKey);
  } else {
    window.localStorage.setItem(prefixedKey, value.toString());
  }
};
const getBoolParam = (key, default_value) => {
  const prefixedKey = `${storageAppName}_${key}`;
  const v = window.localStorage.getItem(prefixedKey);
  if (v === null) {
    return default_value;
  }
  return v.toString().toLowerCase() === 'true';
};
const setBoolParam = (key, value) => {
  const prefixedKey = `${storageAppName}_${key}`;
  if (value === null || value === undefined) {
    window.localStorage.removeItem(prefixedKey);
  } else {
    window.localStorage.setItem(prefixedKey, value.toString());
  }
};
const getStringParam = (key, default_value) => {
  const prefixedKey = `${storageAppName}_${key}`;
  const value = window.localStorage.getItem(prefixedKey);
  return (value === null || value === undefined) ? default_value : value;
};
const setStringParam = (key, value) => {
  const prefixedKey = `${storageAppName}_${key}`;
  if (value === null || value === undefined) {
    window.localStorage.removeItem(prefixedKey);
  } else {
    window.localStorage.setItem(prefixedKey, value.toString());
  }
};

videoBitRate = getIntParam('videoBitRate', videoBitRate);
setIntParam('videoBitRate', videoBitRate);
videoFramerate = getIntParam('videoFramerate', videoFramerate);
setIntParam('videoFramerate', videoFramerate);
videoCRF = getIntParam('videoCRF', videoCRF);
setIntParam('videoCRF', videoCRF);
h264_fullcolor = getBoolParam('h264_fullcolor', h264_fullcolor);
setBoolParam('h264_fullcolor', h264_fullcolor);
h264_streaming_mode = getBoolParam('h264_streaming_mode', h264_streaming_mode);
setBoolParam('h264_streaming_mode', h264_streaming_mode);
jpegQuality = getIntParam('jpegQuality', jpegQuality);
setIntParam('jpegQuality', jpegQuality);
paintOverJpegQuality = getIntParam('paintOverJpegQuality', paintOverJpegQuality);
setIntParam('paintOverJpegQuality', paintOverJpegQuality);
useCpu = getBoolParam('useCpu', useCpu);
setBoolParam('useCpu', useCpu);
h264_paintover_crf = getIntParam('h264_paintover_crf', h264_paintover_crf);
setIntParam('h264_paintover_crf', h264_paintover_crf);
h264_paintover_burst_frames = getIntParam('h264_paintover_burst_frames', h264_paintover_burst_frames);
setIntParam('h264_paintover_burst_frames', h264_paintover_burst_frames);
use_paint_over_quality = getBoolParam('use_paint_over_quality', use_paint_over_quality);
setBoolParam('use_paint_over_quality', use_paint_over_quality);
audioBitRate = getIntParam('audioBitRate', audioBitRate);
setIntParam('audioBitRate', audioBitRate);
resizeRemote = getBoolParam('resizeRemote', resizeRemote);
setBoolParam('resizeRemote', resizeRemote);
debug = getBoolParam('debug', debug);
setBoolParam('debug', debug);
videoBufferSize = getIntParam('videoBufferSize', 0);
setIntParam('videoBufferSize', videoBufferSize);
currentEncoderMode = getStringParam('encoder', 'x264enc');
setStringParam('encoder', currentEncoderMode);
scaleLocallyManual = getBoolParam('scaleLocallyManual', true);
setBoolParam('scaleLocallyManual', scaleLocallyManual);
isManualResolutionMode = getBoolParam('isManualResolutionMode', false);
setBoolParam('isManualResolutionMode', isManualResolutionMode);
isGamepadEnabled = getBoolParam('isGamepadEnabled', true);
setBoolParam('isGamepadEnabled', isGamepadEnabled);
useCssScaling = getBoolParam('useCssScaling', false);
setBoolParam('useCssScaling', useCssScaling);
trackpadMode = getBoolParam('trackpadMode', false);
setBoolParam('trackpadMode', trackpadMode);
scalingDPI = getIntParam('SCALING_DPI', 96);
setIntParam('SCALING_DPI', scalingDPI);
antiAliasingEnabled = getBoolParam('antiAliasingEnabled', true);
setBoolParam('antiAliasingEnabled', antiAliasingEnabled);
useBrowserCursors = getBoolParam('useBrowserCursors', false);
setBoolParam('useBrowserCursors', useBrowserCursors);

if (isSharedMode) {
    manualWidth = 1280;
    manualHeight = 720;
    console.log(`Shared mode: Initialized manualWidth/Height to ${manualWidth}x${manualHeight}`);
} else {
    manualWidth = getIntParam('manualWidth', null);
    setIntParam('manualWidth', manualWidth);
    manualHeight = getIntParam('manualHeight', null);
    setIntParam('manualHeight', manualHeight);
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

const roundDownToEven = (num) => {
  return Math.floor(num / 2) * 2;
};

const updateCanvasImageRendering = () => {
  if (!canvas) return;
  if (!antiAliasingEnabled) {
    if (canvas.style.imageRendering !== 'pixelated') {
      console.log("Anti-aliasing disabled by setting. Forcing 'pixelated' rendering.");
      canvas.style.imageRendering = 'pixelated';
      canvas.style.setProperty('image-rendering', 'crisp-edges', '');
    }
    return;
  }
  const dpr = window.devicePixelRatio || 1;
  if (window.isManualResolutionMode || (useCssScaling && dpr > 1)) {
    if (canvas.style.imageRendering !== 'auto') {
      console.log("Smoothing enabled for manual resolution or high-DPR scaling.");
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
  `;
  document.head.appendChild(style);
};

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
  const dpr = useCssScaling ? 1 : (window.devicePixelRatio || 1);
  const realWidth = roundDownToEven(width * dpr);
  const realHeight = roundDownToEven(height * dpr);
  const resString = `${realWidth}x${realHeight}`;
  console.log(`Sending resolution to server: ${resString}, Pixel Ratio Used: ${dpr}, useCssScaling: ${useCssScaling}`);
  if (websocket && websocket.readyState === WebSocket.OPEN) {
    websocket.send(`r,${resString}`);
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

  const dpr = (window.isManualResolutionMode || useCssScaling) ? 1 : (window.devicePixelRatio || 1);
  const internalBufferWidth = roundDownToEven(targetWidth * dpr);
  const internalBufferHeight = roundDownToEven(targetHeight * dpr);

  if (canvas.width !== internalBufferWidth || canvas.height !== internalBufferHeight) {
    canvas.width = internalBufferWidth;
    canvas.height = internalBufferHeight;
    console.log(`Canvas internal buffer set to: ${internalBufferWidth}x${internalBufferHeight}`);
  }
  const container = canvas.parentElement;
  const containerWidth = container.clientWidth;
  const containerHeight = container.clientHeight;
  if (scaleToFit) {
    const logicalAspectRatio = targetWidth / targetHeight; // Use logical dimensions for aspect ratio calculation
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
    canvas.style.position = 'absolute';
    canvas.style.width = `${cssWidth}px`;
    canvas.style.height = `${cssHeight}px`;
    canvas.style.top = `${topOffset}px`;
    canvas.style.left = `${leftOffset}px`;
    canvas.style.objectFit = 'contain'; // Should be 'fill' if CSS handles aspect ratio
    console.log(`Applied manual style (Scaled): CSS ${cssWidth.toFixed(2)}x${cssHeight.toFixed(2)}, Buffer ${internalBufferWidth}x${internalBufferHeight}, Pos ${leftOffset.toFixed(2)},${topOffset.toFixed(2)}`);
  } else {
    canvas.style.position = 'absolute';
    canvas.style.width = `${targetWidth}px`; // CSS size is logical
    canvas.style.height = `${targetHeight}px`; // CSS size is logical
    canvas.style.top = '0px';
    canvas.style.left = '0px';
    canvas.style.objectFit = 'fill';
    console.log(`Applied manual style (Exact): CSS ${targetWidth}x${targetHeight}, Buffer ${internalBufferWidth}x${internalBufferHeight}, Pos 0,0`);
  }
  canvas.style.display = 'block';
  updateCanvasImageRendering();
}

function resetCanvasStyle(streamWidth, streamHeight) {
  if (!canvas) return;
  if (streamWidth <= 0 || streamHeight <= 0) {
    console.warn(`Cannot reset canvas style: Invalid stream dimensions ${streamWidth}x${streamHeight}`);
    return;
  }

  const dpr = useCssScaling ? 1 : (window.devicePixelRatio || 1); 
  const internalBufferWidth = roundDownToEven(streamWidth * dpr);
  const internalBufferHeight = roundDownToEven(streamHeight * dpr);

  // Set canvas buffer size (internal resolution)
  if (canvas.width !== internalBufferWidth || canvas.height !== internalBufferHeight) {
    canvas.width = internalBufferWidth;
    canvas.height = internalBufferHeight;
    console.log(`Canvas internal buffer reset to: ${internalBufferWidth}x${internalBufferHeight}`);
  }

  // Set canvas CSS display size to explicitly match the logical stream size
  canvas.style.width = `${streamWidth}px`;
  canvas.style.height = `${streamHeight}px`;

  const container = canvas.parentElement;
  if (container) {
    const containerWidth = container.clientWidth;
    const containerHeight = container.clientHeight;

    const leftOffset = Math.floor((containerWidth - streamWidth) / 2);
    const topOffset = Math.floor((containerHeight - streamHeight) / 2);

    canvas.style.position = 'absolute'; // Ensure position is absolute for top/left to work
    canvas.style.top = `${topOffset}px`;
    canvas.style.left = `${leftOffset}px`;
    console.log(`Reset canvas CSS to ${streamWidth}px x ${streamHeight}px, Pos ${leftOffset},${topOffset}, object-fit: fill. Buffer: ${internalBufferWidth}x${internalBufferHeight}`);
  } else {
    canvas.style.position = 'absolute';
    canvas.style.top = '0px';
    canvas.style.left = '0px';
    console.log(`Reset canvas CSS to ${streamWidth}px x ${streamHeight}px, Pos 0,0 (no parent metrics), object-fit: fill. Buffer: ${internalBufferWidth}x${internalBufferHeight}`);
  }

  canvas.style.objectFit = 'fill';
  canvas.style.display = 'block'; // Ensure canvas is displayed
  updateCanvasImageRendering();
}

function enableAutoResize() {
  if (directManualLocalScalingHandler) {
    console.log("Switching to Auto Mode: Removing direct manual local scaling listener.");
    window.removeEventListener('resize', directManualLocalScalingHandler);
  }
  if (originalWindowResizeHandler) {
    console.log("Switching to Auto Mode: Adding original (auto) debounced resize listener.");
    window.removeEventListener('resize', originalWindowResizeHandler); // Ensure no duplicates
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
  // This handler is for non-shared manual mode.
  // Shared mode has its own simpler resize handling in initializeUI.
  if (window.isManualResolutionMode && !isSharedMode && manualWidth != null && manualHeight != null && manualWidth > 0 && manualHeight > 0) {
    applyManualCanvasStyle(manualWidth, manualHeight, scaleLocallyManual);
  }
};

function disableAutoResize() {
  // This is primarily for non-shared manual mode.
  if (originalWindowResizeHandler) {
    console.log("Switching to Manual Mode Local Scaling: Removing original (auto) resize listener.");
    window.removeEventListener('resize', originalWindowResizeHandler);
  }
  console.log("Switching to Manual Mode Local Scaling: Adding direct manual scaling listener.");
  window.removeEventListener('resize', directManualLocalScalingHandler); // Defensive removal
  window.addEventListener('resize', directManualLocalScalingHandler);
  if (window.isManualResolutionMode && !isSharedMode && manualWidth != null && manualHeight != null && manualWidth > 0 && manualHeight > 0) {
    console.log("Applying current manual canvas style after enabling direct manual resize handler.");
    applyManualCanvasStyle(manualWidth, manualHeight, scaleLocallyManual);
  }
}

function updateUIForSharedMode() {
    if (!isSharedMode) return;

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
  overlayInput.type = 'text';
  overlayInput.readOnly = false;
  overlayInput.id = 'overlayInput';
  videoContainer.appendChild(overlayInput);

  videoElement = document.createElement('video');
  videoElement.id = 'stream';
  videoElement.style.display = 'none';
  videoContainer.appendChild(videoElement);

  canvas = document.getElementById('videoCanvas');
  if (!canvas) {
    canvas = document.createElement('canvas');
    canvas.id = 'videoCanvas';
  }
  videoContainer.appendChild(canvas);

  if (isSharedMode) {
      if (!manualWidth || manualWidth <= 0 || !manualHeight || manualHeight <= 0) {
          manualWidth = 1280; manualHeight = 720; // Fallback defaults for safety
      }
      applyManualCanvasStyle(manualWidth, manualHeight, true); // scaleToFit = true
      window.addEventListener('resize', () => { // Simple resize for CSS scaling
          if (isSharedMode && manualWidth && manualHeight && manualWidth > 0 && manualHeight > 0) {
              applyManualCanvasStyle(manualWidth, manualHeight, true);
          }
      });
      console.log(`Initialized UI in Shared Mode: Canvas buffer target ${manualWidth}x${manualHeight} (logical), will scale to fit viewport.`);
  } else if (isManualResolutionMode && manualWidth != null && manualHeight != null && manualWidth > 0 && manualHeight > 0) {
    applyManualCanvasStyle(manualWidth, manualHeight, scaleLocallyManual);
    disableAutoResize(); // Sets up directManualLocalScalingHandler for non-shared manual
    console.log(`Initialized UI in Manual Resolution Mode: ${manualWidth}x${manualHeight} (logical), ScaleLocally: ${scaleLocallyManual}`);
  } else {
    const initialStreamWidth = 1024;
    const initialStreamHeight = 768;
    resetCanvasStyle(initialStreamWidth, initialStreamHeight);
    console.log("Initialized UI in Auto Resolution Mode (defaulting to 1024x768 logical for now)");
  }
  canvasContext = canvas.getContext('2d');
  if (!canvasContext) {
    console.error('Failed to get 2D rendering context');
  }

  audioElement = document.createElement('audio');
  audioElement.id = 'audio_stream';
  audioElement.style.display = 'none';
  videoContainer.appendChild(audioElement);

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
    keyboardInputAssist.type = 'text';
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
      updateUIForSharedMode(); // Call after main UI elements are in DOM
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

function handleDecodedVncStripeFrame(yPos, vncFrameID, frame) {
  decodedStripesQueue.push({
    yPos,
    frame,
    vncFrameID
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
    event.target.value = null; // Clear the input
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
        websocket.send(`FILE_UPLOAD_ERROR:GENERAL:File input processing failed`);
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
  if (detectedSharedModeType === 'shared') {
    inputInitialized = true;
    console.log("Generic #shared mode: Input system instance creation skipped.");
    return;
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

  inputInstance = new Input(overlayInput, sendInputFunction, isSharedMode, playerInputTargetIndex, useCssScaling);

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

  inputInstance.setUseBrowserCursors(useBrowserCursors);

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
        // In shared mode, canvas buffer size is driven by stream dimensions.
        // CSS scaling is re-applied on window resize by a listener in initializeUI.
        if (manualWidth && manualHeight && manualWidth > 0 && manualHeight > 0) {
            applyManualCanvasStyle(manualWidth, manualHeight, true);
        }
        return;
    }
    if (window.isManualResolutionMode) {
      console.log("handleResizeUI: Auto-resize skipped, manual resolution mode is active.");
      return;
    }

    console.log("handleResizeUI: Auto-resize triggered (e.g., by window resize event).");
    const windowResolution = inputInstance.getWindowResolution(); // Returns logical pixels
    const evenWidth = roundDownToEven(windowResolution[0]);
    const evenHeight = roundDownToEven(windowResolution[1]);

    if (evenWidth <= 0 || evenHeight <= 0) {
      console.warn(`handleResizeUI: Calculated invalid dimensions (${evenWidth}x${evenHeight}). Skipping resize send.`);
      return;
    }

    sendResolutionToServer(evenWidth, evenHeight); // Sends DPR-multiplied resolution
    resetCanvasStyle(evenWidth, evenHeight); // Sets DPR-multiplied buffer, logical CSS size
  };

  handleResizeUI_globalRef = handleResizeUI;
  originalWindowResizeHandler = debounce(handleResizeUI, 500);

  if (isSharedMode) {
    console.log("Shared mode: Auto-resize event listener (originalWindowResizeHandler) NOT attached.");
    // Shared mode has its own simple window resize listener in initializeUI for CSS adjustments.
  } else if (!window.isManualResolutionMode) {
    console.log("initializeInput: Auto-resolution mode. Attaching 'resize' event listener for subsequent changes.");
    window.addEventListener('resize', originalWindowResizeHandler);
    const videoContainer = document.querySelector('.video-container');
    let currentAutoWidth, currentAutoHeight;
    if (videoContainer) {
      const rect = videoContainer.getBoundingClientRect();
      currentAutoWidth = roundDownToEven(rect.width); // Logical
      currentAutoHeight = roundDownToEven(rect.height); // Logical
    } else {
      currentAutoWidth = roundDownToEven(window.innerWidth); // Logical
      currentAutoHeight = roundDownToEven(window.innerHeight); // Logical
    }
    if (currentAutoWidth <= 0 || currentAutoHeight <= 0) {
      console.warn(`initializeInput: Current auto-calculated dimensions are invalid (${currentAutoWidth}x${currentAutoHeight}). Defaulting canvas style to 1024x768 (logical) for initial setup. The resolution sent by onopen should prevail on the server.`);
      currentAutoWidth = 1024;
      currentAutoHeight = 768;
    }
    resetCanvasStyle(currentAutoWidth, currentAutoHeight); // Handles DPR internally
    console.log(`initializeInput: Canvas style reset to reflect current auto-dimensions: ${currentAutoWidth}x${currentAutoHeight} (logical). Initial resolution was already sent by onopen.`);
  } else { // Non-shared, manual mode
    console.log("initializeInput: Manual resolution mode active. Initial resolution already sent by onopen.");
    if (manualWidth != null && manualHeight != null && manualWidth > 0 && manualHeight > 0) { // manualWidth/Height are logical
      // applyManualCanvasStyle is called in initializeUI for this case
      disableAutoResize(); // Sets up directManualLocalScalingHandler
    } else {
      console.warn("initializeInput: Manual mode is set, but manualWidth/Height are invalid. Canvas might not display correctly.");
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

  window.webrtcInput = inputInstance;

  const keyboardInputAssist = document.getElementById('keyboard-input-assist');
  if (keyboardInputAssist && inputInstance && !isSharedMode) { // Keyboard assist only for non-shared
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
        if (window.isManualResolutionMode && manualWidth !== null && manualHeight !== null) { // manualWidth/Height are logical
          console.log("Applying new scaling style in manual mode.");
          applyManualCanvasStyle(manualWidth, manualHeight, scaleLocallyManual); // Handles DPR
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
          if (window.isManualResolutionMode && manualWidth != null && manualHeight != null) {
            sendResolutionToServer(manualWidth, manualHeight);
            applyManualCanvasStyle(manualWidth, manualHeight, scaleLocallyManual);
          } else if (!isSharedMode) { // Auto mode
            const currentWindowRes = window.webrtcInput ? window.webrtcInput.getWindowResolution() : [window.innerWidth, window.innerHeight];
            const autoWidth = roundDownToEven(currentWindowRes[0]);
            const autoHeight = roundDownToEven(currentWindowRes[1]);
            sendResolutionToServer(autoWidth, autoHeight);
            resetCanvasStyle(autoWidth, autoHeight);
          } else {
             if (manualWidth && manualHeight) {
                applyManualCanvasStyle(manualWidth, manualHeight, true);
             }
          }
          if (currentEncoderMode !== 'jpeg' && currentEncoderMode !== 'x264enc' && currentEncoderMode !== 'x264enc-striped') {
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
        useBrowserCursors = message.value;
        setBoolParam('useBrowserCursors', useBrowserCursors);
        console.log(`Set useBrowserCursors to ${useBrowserCursors} and persisted.`);
        if (window.webrtcInput && typeof window.webrtcInput.setUseBrowserCursors === 'function') {
          window.webrtcInput.setUseBrowserCursors(useBrowserCursors);
        }
      } else {
        console.warn("Invalid value received for setUseBrowserCursors:", message.value);
      }
      break;
    case 'setManualResolution':
      if (isSharedMode) {
        console.log("Shared mode: setManualResolution message ignored.");
        break;
      }
      const width = parseInt(message.width, 10); // Logical from UI
      const height = parseInt(message.height, 10); // Logical from UI
      if (isNaN(width) || width <= 0 || isNaN(height) || height <= 0) {
        console.error('Received invalid width/height for setManualResolution:', message);
        break;
      }
      console.log(`Setting manual resolution: ${width}x${height} (logical)`);
      window.isManualResolutionMode = true;
      manualWidth = roundDownToEven(width); // Store logical, ensure it's even for consistency if needed by some logic
      manualHeight = roundDownToEven(height); // Store logical
      console.log(`Rounded logical resolution to even numbers: ${manualWidth}x${manualHeight}`);
      setIntParam('manualWidth', manualWidth);
      setIntParam('manualHeight', manualHeight);
      setBoolParam('isManualResolutionMode', true);
      disableAutoResize();
      sendResolutionToServer(manualWidth, manualHeight); // Sends DPR-multiplied
      applyManualCanvasStyle(manualWidth, manualHeight, scaleLocallyManual); // Handles DPR for buffer, uses logical for CSS
      if (currentEncoderMode === 'x264enc' || currentEncoderMode === 'x264enc-striped') {
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
      window.isManualResolutionMode = false;
      manualWidth = null;
      manualHeight = null;
      setIntParam('manualWidth', null);
      setIntParam('manualHeight', null);
      setBoolParam('isManualResolutionMode', false);
      const currentWindowRes = window.webrtcInput ? window.webrtcInput.getWindowResolution() : [window.innerWidth, window.innerHeight]; // Logical
      const autoWidth = roundDownToEven(currentWindowRes[0]); // Logical
      const autoHeight = roundDownToEven(currentWindowRes[1]); // Logical
      resetCanvasStyle(autoWidth, autoHeight); // Handles DPR for buffer, uses logical for CSS
      if (currentEncoderMode === 'x264enc' || currentEncoderMode === 'x264enc-striped') {
        console.log("Clearing VNC stripe decoders due to resolution reset to window.");
        clearAllVncStripeDecoders();
        if (canvasContext) canvasContext.setTransform(1, 0, 0, 1, 0, 0);
        canvasContext.clearRect(0, 0, canvas.width, canvas.height);
      }
      enableAutoResize(); // Will trigger handleResizeUI which sends new (DPR-multiplied) resolution
      break;
    case 'settings':
      console.log('Received settings message:', message.settings);
      handleSettingsMessage(message.settings); // handleSettingsMessage itself gates server sends if isSharedMode
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
      if (websocket && websocket.readyState === WebSocket.OPEN) {
        try {
          const utf8Bytes = new TextEncoder().encode(newClipboardText);
          let binaryString = '';
          for (let i = 0; i < utf8Bytes.length; i++) {
            binaryString += String.fromCharCode(utf8Bytes[i]);
          }
          const encodedText = btoa(binaryString);
          const clipboardMessage = `cw,${encodedText}`;
          websocket.send(clipboardMessage);
          console.log(`Sent clipboard update from UI to server (UTF-8 Base64): cw,...`);
        } catch (e) {
          console.error('Failed to encode or send clipboard text from UI:', e);
        }
      } else {
        console.warn('Cannot send clipboard update from UI: Not connected.');
      }
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
    case 'fileUpload':
      console.log('Received fileUpload message:', message.payload);
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
                // In shared mode, the shared client's gamepad manager should always be enabled for polling,
                window.webrtcInput.gamepadManager.enable();
                console.log("Shared mode: Gamepad control message received, ensuring its GamepadManager remains active for polling.");
            } else {
                // Primary client: respect the toggle
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

function handleSettingsMessage(settings) {
  console.log('Applying settings:', settings);
  if (settings.videoBitRate !== undefined) {
    videoBitRate = parseInt(settings.videoBitRate);
    setIntParam('videoBitRate', videoBitRate);
    if (!isSharedMode && websocket && websocket.readyState === WebSocket.OPEN) {
      const message = `SET_VIDEO_BITRATE,${videoBitRate}`;
      console.log(`Sent websocket message: ${message}`);
      websocket.send(message);
    } else if (!isSharedMode) {
      console.warn("Websocket connection not open, cannot send video bitrate setting.");
    }
  }
  if (settings.videoFramerate !== undefined) {
    videoFramerate = parseInt(settings.videoFramerate);
    setIntParam('videoFramerate', videoFramerate);
    if (!isSharedMode && websocket && websocket.readyState === WebSocket.OPEN) {
      const message = `SET_FRAMERATE,${videoFramerate}`;
      console.log(`Sent websocket message: ${message}`);
      websocket.send(message);
    } else if (!isSharedMode) {
      console.warn("Websocket connection not open, cannot send framerate setting.");
    }
  }
  if (settings.resizeRemote !== undefined) {
    resizeRemote = settings.resizeRemote;
    setBoolParam('resizeRemote', resizeRemote);
    console.warn("ResizeRemote setting received; for websockets, server ENABLE_RESIZE and client 'r,' messages control resizing.");
  }
  if (settings.encoder !== undefined) {
    const newEncoderSetting = settings.encoder;
    const oldEncoderActual = currentEncoderMode;

    if (oldEncoderActual !== newEncoderSetting) {
      currentEncoderMode = newEncoderSetting;
      setStringParam('encoder', currentEncoderMode);

      if (!isSharedMode && websocket && websocket.readyState === WebSocket.OPEN) {
        const message = `SET_ENCODER,${currentEncoderMode}`;
        console.log(`Sent websocket message: ${message}`);
        websocket.send(message);
      } else if (!isSharedMode) {
        console.warn("Websocket connection not open, cannot send encoder setting.");
      }

      const isNewPixelfluxH264 = newEncoderSetting === 'x264enc' || newEncoderSetting === 'x264enc-striped';
      const isOldPixelfluxH264 = oldEncoderActual === 'x264enc' || oldEncoderActual === 'x264enc-striped';
      const isNewJpeg = newEncoderSetting === 'jpeg';
      const isOldJpeg = oldEncoderActual === 'jpeg';
      const isNewVideoPipeline = !isNewJpeg && !isNewPixelfluxH264;
      const isOldVideoPipeline = !isOldJpeg && !isOldPixelfluxH264;
      const isOldStripedH264 = oldEncoderActual === 'x264enc-striped';
      const isNewStripedH264 = newEncoderSetting === 'x264enc-striped';

      if (isOldStripedH264 && !isNewStripedH264) {
        clearAllVncStripeDecoders();
      }
      if ((isOldVideoPipeline || isOldStripedH264) && isNewJpeg) {
        if (decoder && decoder.state !== 'closed') {
          console.log(`Switching from ${oldEncoderActual} to JPEG, closing main video decoder.`);
          decoder.close();
          decoder = null;
        }
      }

      if (isNewPixelfluxH264) {
        if (canvasContext) {
          canvasContext.setTransform(1, 0, 0, 1, 0, 0);
          canvasContext.clearRect(0, 0, canvas.width, canvas.height);
          console.log("Switched to x264enc-striped, cleared canvas.");
        }
        if (decoder && decoder.state !== 'closed') {
          console.log("Switching to x264enc-striped, closing main video decoder.");
          decoder.close();
          decoder = null;
        }
      } else if (isNewJpeg) {
        console.log("Encoder changed to JPEG. Ensuring canvas buffer is correctly sized.");
        let currentTargetWidth, currentTargetHeight; // Logical dimensions
        if (isSharedMode) { // In shared mode, use its manualWidth/Height (logical)
            currentTargetWidth = manualWidth;
            currentTargetHeight = manualHeight;
            if (currentTargetWidth && currentTargetHeight) applyManualCanvasStyle(currentTargetWidth, currentTargetHeight, true); // Handles DPR

        } else if (window.isManualResolutionMode && manualWidth != null && manualHeight != null) { // manualWidth/Height are logical
          currentTargetWidth = manualWidth;
          currentTargetHeight = manualHeight;
          applyManualCanvasStyle(currentTargetWidth, currentTargetHeight, scaleLocallyManual); // Handles DPR
        } else {
          if (window.webrtcInput && typeof window.webrtcInput.getWindowResolution === 'function') {
            const currentWindowRes = window.webrtcInput.getWindowResolution(); // Logical
            currentTargetWidth = roundDownToEven(currentWindowRes[0]);
            currentTargetHeight = roundDownToEven(currentWindowRes[1]);
            resetCanvasStyle(currentTargetWidth, currentTargetHeight); // Handles DPR
          } else {
            console.warn("Cannot determine auto resolution for JPEG switch: webrtcInput or getWindowResolution not available.");
          }
        }
      } else if (isNewVideoPipeline) {
        console.log(`Switching to video pipeline ${newEncoderSetting}. Ensuring main decoder is initialized/reconfigured.`);
        triggerInitializeDecoder(); // Uses logical dimensions internally and applies DPR
      }
      const wasStripedOrJpeg = isOldJpeg || isOldStripedH264;
      if (wasStripedOrJpeg && isNewVideoPipeline) {
        console.log(`Switched from ${oldEncoderActual} (striped/jpeg) to ${newEncoderSetting} (video pipeline). Resending video bitrate ${videoBitRate} kbit/s.`);
        if (!isSharedMode && websocket && websocket.readyState === WebSocket.OPEN) {
          const message = `SET_VIDEO_BITRATE,${videoBitRate}`;
          websocket.send(message);
        }
      }
    } else {
      console.log(`Encoder setting received (${newEncoderSetting}), but it's the same as current (${oldEncoderActual}). No change.`);
    }
  }
  if (settings.videoBufferSize !== undefined) {
    videoBufferSize = parseInt(settings.videoBufferSize);
    setIntParam('videoBufferSize', videoBufferSize);
    console.log(`Applied Video buffer size setting: ${videoBufferSize} frames.`);
  }
  if (settings.videoCRF !== undefined) {
    videoCRF = parseInt(settings.videoCRF, 10);
    setIntParam('videoCRF', videoCRF);
    console.log(`Applied Video CRF setting: ${videoCRF}.`);
    if (!isSharedMode && websocket && websocket.readyState === WebSocket.OPEN) {
      const message = `SET_CRF,${videoCRF}`;
      console.log(`Sent websocket message: ${message}`);
      websocket.send(message);
    } else if (!isSharedMode) {
      console.warn("Websocket connection not open, cannot send CRF setting.");
    }
  }
  if (settings.h264_fullcolor !== undefined) {
    const newFullColorValue = !!settings.h264_fullcolor;
    if (h264_fullcolor !== newFullColorValue) {
      h264_fullcolor = newFullColorValue;
      setBoolParam('h264_fullcolor', h264_fullcolor);
      console.log(`Applied H.264 Full Color setting: ${h264_fullcolor}.`);
      if (!isSharedMode && websocket && websocket.readyState === WebSocket.OPEN && (currentEncoderMode === 'x264enc' || currentEncoderMode === 'x264enc-striped')) {
        const message = `SET_H264_FULLCOLOR,${h264_fullcolor}`;
        console.log(`Sent websocket message: ${message}`);
        websocket.send(message);
      } else if (!isSharedMode && currentEncoderMode !== 'x264enc' && currentEncoderMode !== 'x264enc-striped') {
        console.log("H.264 Full Color setting changed, but current encoder is not x264enc-striped. WebSocket command not sent.");
      } else if (!isSharedMode) {
        console.warn("Websocket connection not open, cannot send H.264 Full Color setting.");
      }
    } else {
      console.log(`H.264 Full Color setting received (${newFullColorValue}), but it's the same as current. No change.`);
    }
  }
  if (settings.h264_streaming_mode !== undefined) {
    const newStreamingModeValue = !!settings.h264_streaming_mode;
    if (h264_streaming_mode !== newStreamingModeValue) {
      h264_streaming_mode = newStreamingModeValue;
      setBoolParam('h264_streaming_mode', h264_streaming_mode);
      console.log(`Applied H.264 Streaming Mode setting: ${h264_streaming_mode}.`);
      if (!isSharedMode && websocket && websocket.readyState === WebSocket.OPEN && (currentEncoderMode === 'x264enc' || currentEncoderMode === 'x264enc-striped')) {
        const message = `SET_H264_STREAMING_MODE,${h264_streaming_mode}`;
        console.log(`Sent websocket message: ${message}`);
        websocket.send(message);
      } else if (!isSharedMode && currentEncoderMode !== 'x264enc' && currentEncoderMode !== 'x264enc-striped') {
        console.log("H.264 Streaming Mode setting changed, but current encoder is not x264enc-striped. WebSocket command not sent.");
      } else if (!isSharedMode) {
        console.warn("Websocket connection not open, cannot send H.264 Streaming Mode setting.");
      }
    } else {
      console.log(`H.264 Streaming Mode setting received (${newStreamingModeValue}), but it's the same as current. No change.`);
    }
  }
  if (settings.jpegQuality !== undefined) {
    const newQuality = parseInt(settings.jpegQuality, 10);
    if (jpegQuality !== newQuality) {
      jpegQuality = newQuality;
      setIntParam('jpegQuality', jpegQuality);
      console.log(`Applied JPEG Quality setting: ${jpegQuality}.`);
      if (!isSharedMode && websocket && websocket.readyState === WebSocket.OPEN && currentEncoderMode === 'jpeg') {
        const message = `SET_JPEG_QUALITY,${jpegQuality}`;
        console.log(`Sent websocket message: ${message}`);
        websocket.send(message);
      } else if (!isSharedMode && currentEncoderMode !== 'jpeg') {
        console.log("JPEG Quality setting changed, but current encoder is not 'jpeg'. WebSocket command not sent.");
      } else if (!isSharedMode) {
        console.warn("Websocket connection not open, cannot send JPEG Quality setting.");
      }
    } else {
      console.log(`JPEG Quality setting received (${newQuality}), but it's the same as current. No change.`);
    }
  }
  if (settings.paintOverJpegQuality !== undefined) {
    const newQuality = parseInt(settings.paintOverJpegQuality, 10);
    if (paintOverJpegQuality !== newQuality) {
      paintOverJpegQuality = newQuality;
      setIntParam('paintOverJpegQuality', paintOverJpegQuality);
      console.log(`Applied Paint-Over JPEG Quality setting: ${paintOverJpegQuality}.`);
      if (!isSharedMode && websocket && websocket.readyState === WebSocket.OPEN && currentEncoderMode === 'jpeg') {
        const message = `SET_PAINT_OVER_JPEG_QUALITY,${paintOverJpegQuality}`;
        console.log(`Sent websocket message: ${message}`);
        websocket.send(message);
      } else if (!isSharedMode && currentEncoderMode !== 'jpeg') {
        console.log("Paint-Over JPEG Quality setting changed, but current encoder is not 'jpeg'. WebSocket command not sent.");
      } else if (!isSharedMode) {
        console.warn("Websocket connection not open, cannot send Paint-Over JPEG Quality setting.");
      }
    } else {
      console.log(`Paint-Over JPEG Quality setting received (${newQuality}), but it's the same as current. No change.`);
    }
  }
  if (settings.useCpu !== undefined) {
    const newUseCpu = !!settings.useCpu;
    if (useCpu !== newUseCpu) {
      useCpu = newUseCpu;
      setBoolParam('useCpu', useCpu);
      console.log(`Applied Use CPU setting: ${useCpu}.`);
      const isPixelfluxH264 = currentEncoderMode === 'x264enc' || currentEncoderMode === 'x264enc-striped';
      if (!isSharedMode && websocket && websocket.readyState === WebSocket.OPEN && isPixelfluxH264) {
        const message = `SET_USE_CPU,${useCpu}`;
        console.log(`Sent websocket message: ${message}`);
        websocket.send(message);
      } else if (!isSharedMode && !isPixelfluxH264) {
        console.log("Use CPU setting changed, but current encoder is not a Pixelflux H.264 encoder. WebSocket command not sent.");
      } else if (!isSharedMode) {
        console.warn("Websocket connection not open, cannot send Use CPU setting.");
      }
    } else {
      console.log(`Use CPU setting received (${newUseCpu}), but it's the same as current. No change.`);
    }
  }
  if (settings.h264_paintover_crf !== undefined) {
    const newCrf = parseInt(settings.h264_paintover_crf, 10);
    if (h264_paintover_crf !== newCrf) {
      h264_paintover_crf = newCrf;
      setIntParam('h264_paintover_crf', h264_paintover_crf);
      console.log(`Applied H.264 Paint-Over CRF setting: ${h264_paintover_crf}.`);
      const isPixelfluxH264 = currentEncoderMode === 'x264enc' || currentEncoderMode === 'x264enc-striped';
      if (!isSharedMode && websocket && websocket.readyState === WebSocket.OPEN && isPixelfluxH264) {
        const message = `SET_H264_PAINTOVER_CRF,${h264_paintover_crf}`;
        console.log(`Sent websocket message: ${message}`);
        websocket.send(message);
      } else if (!isSharedMode && !isPixelfluxH264) {
        console.log("H.264 Paint-Over CRF setting changed, but current encoder is not a Pixelflux H.264 encoder. WebSocket command not sent.");
      } else if (!isSharedMode) {
        console.warn("Websocket connection not open, cannot send H.264 Paint-Over CRF setting.");
      }
    } else {
      console.log(`H.264 Paint-Over CRF setting received (${newCrf}), but it's the same as current. No change.`);
    }
  }
  if (settings.h264_paintover_burst_frames !== undefined) {
    const newBurst = parseInt(settings.h264_paintover_burst_frames, 10);
    if (h264_paintover_burst_frames !== newBurst) {
      h264_paintover_burst_frames = newBurst;
      setIntParam('h264_paintover_burst_frames', h264_paintover_burst_frames);
      console.log(`Applied H.264 Paint-Over Burst Frames setting: ${h264_paintover_burst_frames}.`);
      const isPixelfluxH264 = currentEncoderMode === 'x264enc' || currentEncoderMode === 'x264enc-striped';
      if (!isSharedMode && websocket && websocket.readyState === WebSocket.OPEN && isPixelfluxH264) {
        const message = `SET_H264_PAINTOVER_BURST_FRAMES,${h264_paintover_burst_frames}`;
        console.log(`Sent websocket message: ${message}`);
        websocket.send(message);
      } else if (!isSharedMode && !isPixelfluxH264) {
        console.log("H.264 Paint-Over Burst Frames setting changed, but current encoder is not a Pixelflux H.264 encoder. WebSocket command not sent.");
      } else if (!isSharedMode) {
        console.warn("Websocket connection not open, cannot send H.264 Paint-Over Burst Frames setting.");
      }
    } else {
      console.log(`H.264 Paint-Over Burst Frames setting received (${newBurst}), but it's the same as current. No change.`);
    }
  }
  if (settings.use_paint_over_quality !== undefined) {
    const newUsePaintOver = !!settings.use_paint_over_quality;
    if (use_paint_over_quality !== newUsePaintOver) {
      use_paint_over_quality = newUsePaintOver;
      setBoolParam('use_paint_over_quality', use_paint_over_quality);
      console.log(`Applied Use Paint-Over Quality setting: ${use_paint_over_quality}.`);
      const isApplicableEncoder = currentEncoderMode === 'jpeg' || currentEncoderMode === 'x264enc' || currentEncoderMode === 'x264enc-striped';
      if (!isSharedMode && websocket && websocket.readyState === WebSocket.OPEN && isApplicableEncoder) {
        const message = `SET_USE_PAINT_OVER_QUALITY,${use_paint_over_quality}`;
        console.log(`Sent websocket message: ${message}`);
        websocket.send(message);
      } else if (!isSharedMode && !isApplicableEncoder) {
        console.log("Use Paint-Over Quality setting changed, but current encoder is not applicable. WebSocket command not sent.");
      } else if (!isSharedMode) {
        console.warn("Websocket connection not open, cannot send Use Paint-Over Quality setting.");
      }
    } else {
      console.log(`Use Paint-Over Quality setting received (${newUsePaintOver}), but it's the same as current. No change.`);
    }
  }
  if (settings.SCALING_DPI !== undefined) {
    const dpi = parseInt(settings.SCALING_DPI, 10);
    if (!isNaN(dpi)) {
      scalingDPI = dpi;
      setIntParam('SCALING_DPI', scalingDPI);
      console.log(`Applied SCALING_DPI setting: ${dpi}.`);
      if (!isSharedMode && websocket && websocket.readyState === WebSocket.OPEN) {
        const message = `s,${dpi}`;
        console.log(`Sent websocket message: ${message}`);
        websocket.send(message);
      } else if (isSharedMode) {
        console.log("SCALING_DPI setting ignored in shared mode.");
      } else {
        console.warn("Websocket connection not open, cannot send SCALING_DPI setting.");
      }
    } else {
      console.warn(`Invalid SCALING_DPI value received: ${settings.SCALING_DPI}`);
    }
  }
  if (settings.turnSwitch !== undefined) {
    console.log(`turnSwitch setting received (WebRTC specific): ${settings.turnSwitch}. No action in WebSocket mode.`);
  }
  if (settings.debug !== undefined) {
    debug = settings.debug;
    setBoolParam('debug', debug);
    console.log(`Applied debug setting: ${debug}. Reloading...`);
    setTimeout(() => {
      window.location.reload();
    }, 700);
  }
}

function sendStatsMessage() {
  const stats = {
    connection: connectionStat,
    gpu: gpuStat,
    cpu: cpuStat,
    network: networkStat,
    clientFps: window.fps,
    audioBuffer: window.currentAudioBufferSize,
    videoBuffer: videoFrameBuffer.length,
    isVideoPipelineActive: isVideoPipelineActive,
    isAudioPipelineActive: isAudioPipelineActive,
    isMicrophoneActive: isMicrophoneActive,
  };
  stats.encoderName = currentEncoderMode;
  stats.h264_fullcolor = h264_fullcolor;
  stats.h264_streaming_mode = h264_streaming_mode;
  window.parent.postMessage({
    type: 'stats',
    data: stats
  }, window.location.origin);
  console.log('Sent stats message via window.postMessage:', stats);
}

function startSharedModeProbingTimeout() {
    clearTimeout(sharedProbingTimeoutId);
    sharedProbingTimeoutId = setTimeout(() => {
        console.warn(`Shared mode (${detectedSharedModeType}): Timeout waiting for video identification packet (attempt ${sharedProbingAttempts + 1}/${MAX_SHARED_PROBING_ATTEMPTS}).`);
        sharedProbingAttempts++;
        if (sharedProbingAttempts < MAX_SHARED_PROBING_ATTEMPTS) {
            if (sharedClientState === 'awaiting_identification') {
                console.log(`Shared mode (${detectedSharedModeType}): Probing timeout. Attempting to re-trigger stream with STOP/START_VIDEO.`);
                // Attempt to re-trigger the stream
                if (websocket && websocket.readyState === WebSocket.OPEN) {
                    websocket.send('STOP_VIDEO');
                    setTimeout(() => {
                        if (websocket && websocket.readyState === WebSocket.OPEN) {
                            websocket.send('START_VIDEO');
                            console.log(`Shared mode (${detectedSharedModeType}): Sent START_VIDEO after probing timeout.`);
                        }
                    }, 250);
                }
                startSharedModeProbingTimeout(); // Restart timeout for the new attempt
            } else {
                 console.log(`Shared mode: Probing timeout fired but state is ${sharedClientState}. Not retrying automatically.`);
            }
        } else {
            console.error("Shared mode: Failed to identify video type after multiple attempts. Entering error state. Stream may not be active or correctly configured on server/primary client.");
            sharedClientState = 'error';
            // Display an error to the user if possible, or just log.
            if (statusDisplayElement) {
                statusDisplayElement.textContent = 'Error: Could not identify video stream.';
                statusDisplayElement.classList.remove('hidden');
            }
        }
    }, SHARED_PROBING_TIMEOUT_MS);
}

function clearSharedModeProbingTimeout() {
    clearTimeout(sharedProbingTimeoutId);
    sharedProbingTimeoutId = null;
}


document.addEventListener('DOMContentLoaded', () => {
  async function initializeDecoder() {
    if (decoder && decoder.state !== 'closed') {
      console.warn("VideoDecoder already exists, closing before re-initializing.");
      decoder.close();
    }
    let targetWidth = 1024;
    let targetHeight = 768;
    if (isSharedMode) {
        targetWidth = manualWidth > 0 ? manualWidth : 1024;
        targetHeight = manualHeight > 0 ? manualHeight : 768;
    } else if (window.isManualResolutionMode && manualWidth != null && manualHeight != null) {
      targetWidth = manualWidth;
      targetHeight = manualHeight;
    } else if (window.webrtcInput && typeof window.webrtcInput.getWindowResolution === 'function') {
      try {
        const currentRes = window.webrtcInput.getWindowResolution();
        const autoWidth = roundDownToEven(currentRes[0]);
        const autoHeight = roundDownToEven(currentRes[1]);
        if (autoWidth > 0 && autoHeight > 0) {
          targetWidth = autoWidth;
          targetHeight = autoHeight;
        }
      } catch (e) { /* use defaults */ }
    }

    const dpr = useCssScaling ? 1 : (window.devicePixelRatio || 1);
    const actualCodedWidth = roundDownToEven(targetWidth * dpr);
    const actualCodedHeight = roundDownToEven(targetHeight * dpr);

    decoder = new VideoDecoder({
      output: handleDecodedFrame,
      error: (e) => initiateFallback(e, 'main_decoder'),
    });
    const decoderConfig = {
      codec: 'avc1.42E01E',
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
      console.log('Main VideoDecoder configured successfully with config:', decoderConfig);
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

  window.addEventListener('focus', () => {
    if (isSharedMode) return;
    navigator.clipboard
      .readText()
      .then((text) => {
        const utf8Bytes = new TextEncoder().encode(text);
        let binaryString = '';
        for (let i = 0; i < utf8Bytes.length; i++) {
          binaryString += String.fromCharCode(utf8Bytes[i]);
        }
        const encodedText = btoa(binaryString);
        if (websocket && websocket.readyState === WebSocket.OPEN) {
          websocket.send(`cw,${encodedText}`); // Clipboard write
          console.log("Sent clipboard on focus (UTF-8 Base64)");
        }
      })
      .catch((err) => {
        console.error(`Failed to read clipboard contents on focus: ${err}`);
      });
  });

  document.addEventListener('visibilitychange', async () => {
    if (isSharedMode) {
      console.log("Shared mode: Tab visibility changed, stream control bypassed. Current state:", document.hidden ? "hidden" : "visible");
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
      if (websocket && websocket.readyState === WebSocket.OPEN) {
        if (!isVideoPipelineActive) {
          websocket.send('START_VIDEO');
          // Re-acquire the wake lock if it was released.
          if (wakeLockSentinel === null) {
            console.log('Tab is visible again, re-acquiring Wake Lock.');
            await requestWakeLock();
          }
          isVideoPipelineActive = true;
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

  async function decodeAndQueueJpegStripe(startY, jpegData) {
    if (typeof ImageDecoder === 'undefined') {
      console.warn('ImageDecoder API not supported. Cannot decode JPEG stripes.');
      return;
    }
    try {
      const imageDecoder = new ImageDecoder({
        data: jpegData,
        type: 'image/jpeg'
      });
      const result = await imageDecoder.decode();
      jpegStripeRenderQueue.push({
        image: result.image,
        startY: startY
      });
      imageDecoder.close();
    } catch (error) {
      console.error('Error decoding JPEG stripe:', error, 'startY:', startY, 'dataLength:', jpegData.byteLength);
    }
  }

function handleDecodedFrame(frame) { // frame.codedWidth/Height are physical pixels
    const isGStreamerH264Mode =
        (currentEncoderMode !== 'jpeg' && currentEncoderMode !== 'x264enc-striped' && currentEncoderMode !== 'x264enc' && !isSharedMode) ||
        (isSharedMode && identifiedEncoderModeForShared === 'h264_full_frame');

    // close the frame immediately to prevent memory buildup.
    if (document.hidden && isGStreamerH264Mode) {
        frame.close();
        return; // Do not process or buffer this frame further
    }

    if (!isSharedMode && clientMode === 'websockets' && !isVideoPipelineActive) {
        frame.close();
        return;
    }

    if (isSharedMode && identifiedEncoderModeForShared === 'h264_full_frame' && sharedClientState === 'ready') {
        const dpr_for_conversion = useCssScaling ? 1 : (window.devicePixelRatio || 1);
        const physicalFrameWidth = frame.codedWidth; // Physical
        const physicalFrameHeight = frame.codedHeight; // Physical

        // Convert physical frame dimensions to logical for comparison and storage in manualWidth/Height
        const logicalFrameWidth = physicalFrameWidth / dpr;
        const logicalFrameHeight = physicalFrameHeight / dpr;

        if ((manualWidth !== logicalFrameWidth || manualHeight !== logicalFrameHeight) && logicalFrameWidth > 0 && logicalFrameHeight > 0) {
            manualWidth = logicalFrameWidth; // Store as logical
            manualHeight = logicalFrameHeight; // Store as logical
            console.log(`Shared mode (decoded H264): Updated manual (logical) dimensions from H.264 frame to ${manualWidth.toFixed(2)}x${manualHeight.toFixed(2)} (Physical: ${physicalFrameWidth}x${physicalFrameHeight})`);
            applyManualCanvasStyle(manualWidth, manualHeight, true); // applyManualCanvasStyle takes logical, handles DPR
        }
    }

    if (isGStreamerH264Mode) {
        videoFrameBuffer.push(frame);
    } else {
        console.warn(`[handleDecodedFrame] Frame received but not for a GStreamer H.264 mode that uses videoFrameBuffer. isSharedMode: ${isSharedMode}, currentEncoderMode: ${currentEncoderMode}, identifiedEncoderModeForShared: ${identifiedEncoderModeForShared}. Closing frame to be safe.`);
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

    const dpr = window.devicePixelRatio || 1;
    const dpr_for_conversion = useCssScaling ? 1 : dpr;

    if (isSharedMode) {
      // manualWidth/Height are logical. applyManualCanvasStyle calculates physical buffer size.
      if (manualWidth && manualHeight && manualWidth > 0 && manualHeight > 0) {
          const expectedPhysicalCanvasWidth = roundDownToEven(manualWidth * dpr);
          const expectedPhysicalCanvasHeight = roundDownToEven(manualHeight * dpr);
          if (canvas.width !== expectedPhysicalCanvasWidth || canvas.height !== expectedPhysicalCanvasHeight) {
            console.log(`Shared mode (paintVideoFrame): Canvas buffer ${canvas.width}x${canvas.height} out of sync with expected physical ${expectedPhysicalCanvasWidth}x${expectedPhysicalCanvasHeight} (logical: ${manualWidth}x${manualHeight}). Re-applying style.`);
            applyManualCanvasStyle(manualWidth, manualHeight, true); // Takes logical, handles DPR
          }
      }
    }

    let videoPaintedThisFrame = false;
    let jpegPaintedThisFrame = false;

    if (currentEncoderMode === 'x264enc' || currentEncoderMode === 'x264enc-striped') {
      if (isSharedMode && sharedClientState === 'ready' && decodedStripesQueue.length > 0) {
          const firstStripeFrame = decodedStripesQueue[0].frame;
          if (firstStripeFrame && firstStripeFrame.codedWidth > 0) {
              const physicalStripeCodedWidth = firstStripeFrame.codedWidth; // This is the stripe's own coded width
              const logicalStripeCodedWidth = physicalStripeCodedWidth / dpr_for_conversion; // Convert to logical
              if (manualWidth !== logicalStripeCodedWidth && logicalStripeCodedWidth > 0) {
                  manualWidth = logicalStripeCodedWidth; // Store as logical
                  console.log(`Shared mode (VNC stripe paint): Updated manual (logical) Width from VNC stripe to ${manualWidth.toFixed(2)} (Stripe Coded: ${physicalStripeCodedWidth}, DPR for conversion: ${dpr_for_conversion})`);
                  if (manualHeight && manualWidth > 0 && manualHeight > 0) {
                      applyManualCanvasStyle(manualWidth, manualHeight, true); // Takes logical
                  }
              }
          }
      }
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
    } else if (currentEncoderMode === 'jpeg') {
      if (canvasContext && jpegStripeRenderQueue.length > 0) {
        if (isSharedMode && sharedClientState === 'ready' && jpegStripeRenderQueue.length > 0) {
            const firstStripeImage = jpegStripeRenderQueue[0].image;
            if (firstStripeImage && firstStripeImage.codedWidth > 0) {
                const physicalImageCodedWidth = firstStripeImage.codedWidth; // Image's own coded width
                const logicalImageCodedWidth = physicalImageCodedWidth / dpr_for_conversion; // Convert to logical
                if (manualWidth !== logicalImageCodedWidth && logicalImageCodedWidth > 0) {
                    manualWidth = logicalImageCodedWidth; // Store as logical
                    console.log(`Shared mode (JPEG stripe paint): Updated manual (logical) Width from JPEG stripe to ${manualWidth.toFixed(2)} (Image Coded: ${physicalImageCodedWidth}, DPR for conversion: ${dpr_for_conversion})`);
                    if (manualHeight && manualWidth > 0 && manualHeight > 0) {
                        applyManualCanvasStyle(manualWidth, manualHeight, true); // Takes logical
                    }
                }
            }
        }
        if ((canvas.width === 0 || canvas.height === 0) || (canvas.width === 300 && canvas.height === 150)) {
          const firstStripe = jpegStripeRenderQueue[0];
          if (firstStripe && firstStripe.image && (firstStripe.startY + firstStripe.image.height > canvas.height || firstStripe.image.width > canvas.width)) {
            console.warn(`[paintVideoFrame] Canvas dimensions (${canvas.width}x${canvas.height}) may be too small for JPEG stripes.`);
          }
        }
        while (jpegStripeRenderQueue.length > 0) {
          const segment = jpegStripeRenderQueue.shift();
          if (segment && segment.image) {
            try {
              if (canvas.width > 0 && canvas.height > 0) {
                canvasContext.drawImage(segment.image, 0, segment.startY);
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
    } else if ( (isSharedMode && currentEncoderMode === 'h264_full_frame' && sharedClientState === 'ready') ||
                (!isSharedMode && currentEncoderMode !== 'jpeg' && currentEncoderMode !== 'x264enc' && currentEncoderMode !== 'x264enc-striped') ) {
      if (!document.hidden || (isSharedMode && sharedClientState === 'ready')) {
        if ( (isSharedMode && sharedClientState === 'ready') || (!isSharedMode && isVideoPipelineActive) ) {
           const bufferLimit = (isSharedMode && sharedClientState === 'ready') ? 0 : videoBufferSize;
           if (videoFrameBuffer.length > bufferLimit) {
                const frameToPaint = videoFrameBuffer.shift();
                if (frameToPaint) {
                    if (canvas.width > 0 && canvas.height > 0) {
                        canvasContext.drawImage(frameToPaint, 0, 0);
                    }
                    frameToPaint.close();
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
    if (!audioContext) {
      const contextOptions = {
        sampleRate: 48000
      };
      audioContext = new(window.AudioContext || window.webkitAudioContext)(contextOptions);
      console.log('Playback AudioContext initialized. Actual sampleRate:', audioContext.sampleRate, 'Initial state:', audioContext.state);
      audioContext.onstatechange = () => {
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
                this.audioBufferQueue = [];
                this.currentAudioData = null;
                this.currentDataOffset = 0;

                this.TARGET_BUFFER_PACKETS = 3;
                this.MAX_BUFFER_PACKETS = 8;

                this.port.onmessage = (event) => {
                    if (event.data.audioData) {
                        const pcmData = new Float32Array(event.data.audioData);
                        if (this.audioBufferQueue.length >= this.MAX_BUFFER_PACKETS) {
                            this.audioBufferQueue.shift();
                        }
                        this.audioBufferQueue.push(pcmData);
                    } else if (event.data.type === 'getBufferSize') {
                        const bufferMillis = this.audioBufferQueue.reduce((total, buf) => total + (buf.length / 2 / sampleRate) * 1000, 0);
                        this.port.postMessage({
                            type: 'audioBufferSize',
                            size: this.audioBufferQueue.length,
                            durationMs: bufferMillis
                        });
                    }
                };
            }

            process(inputs, outputs, parameters) {
                const output = outputs[0];
                const leftChannel = output ? output[0] : undefined;

                if (!leftChannel) {
                    return true;
                }
                
                const rightChannel = output ? output[1] : leftChannel;
                const samplesPerBuffer = leftChannel.length;

                if (this.audioBufferQueue.length === 0 && this.currentAudioData === null) {
                    leftChannel.fill(0);
                    rightChannel.fill(0);
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
                            leftChannel.fill(0, sampleIndex);
                            rightChannel.fill(0, sampleIndex);
                            return true;
                        }
                    }
                    
                    leftChannel[sampleIndex] = data[offset++];
                    if (offset < data.length) {
                        rightChannel[sampleIndex] = data[offset++];
                    } else {
                        rightChannel[sampleIndex] = leftChannel[sampleIndex];
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
      audioWorkletNode = new AudioWorkletNode(audioContext, 'audio-frame-processor', {
        numberOfOutputs: 1,
        outputChannelCount: [2]
      });
      audioWorkletProcessorPort = audioWorkletNode.port;
      audioWorkletProcessorPort.onmessage = (event) => {
        if (event.data.type === 'audioBufferSize') {
            window.currentAudioBufferSize = event.data.size;
            window.currentAudioBufferDuration = event.data.durationMs;
        }
      };
      audioWorkletNode.connect(audioContext.destination);
      console.log('Playback AudioWorkletProcessor initialized and connected.');
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
            if (window.currentAudioBufferSize < 10) { // Keep buffer low for Opus
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
        audioDecoderWorker.postMessage({
          type: 'init',
          data: {
            initialPipelineStatus: isAudioPipelineActive // Or true for shared mode initially
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
  const websocketEndpointURL = new URL(`${ws_protocol}${window.location.host}${pathname}websockets`);
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

  const sendHeartbeat = () => {
    if (websocket && websocket.readyState === WebSocket.OPEN) {
      try {
        websocket.send('PING');
        console.log('[Heartbeat] Sent PING to keep WebSocket connection alive');
      } catch (error) {
        console.error('[Heartbeat] Error sending PING:', error);
      }
    }
  };

  const sendClientMetrics = () => {
    if (isSharedMode) return; // Shared mode does not have client-side FPS display in this context

    if (isSidebarOpen) {
      const now = performance.now();
      const elapsedStriped = now - lastStripedFpsUpdateTime;
      const elapsedFullFrame = now - lastFpsUpdateTime;
      const fpsUpdateInterval = 1000; // ms

      if (uniqueStripedFrameIdsThisPeriod.size > 0) {
        if (elapsedStriped >= fpsUpdateInterval) {
          const stripedFps = (uniqueStripedFrameIdsThisPeriod.size * 1000) / elapsedStriped;
          window.fps = Math.round(stripedFps);
          uniqueStripedFrameIdsThisPeriod.clear();
          lastStripedFpsUpdateTime = now;
          frameCount = 0; // Reset full frame count as striped is primary
          lastFpsUpdateTime = now; // Also reset its timer
        }
      } else if (frameCount > 0) {
        if (elapsedFullFrame >= fpsUpdateInterval) {
          const fullFrameFps = (frameCount * 1000) / elapsedFullFrame;
          window.fps = Math.round(fullFrameFps);
          frameCount = 0;
          lastFpsUpdateTime = now;
          lastStripedFpsUpdateTime = now; // Reset its timer too
        }
      } else {
        if (elapsedStriped >= fpsUpdateInterval || elapsedFullFrame >= fpsUpdateInterval) {
             window.fps = 0;
             lastFpsUpdateTime = now;
             lastStripedFpsUpdateTime = now;
        }
      }

      if (audioWorkletProcessorPort) {
        audioWorkletProcessorPort.postMessage({
          type: 'getBufferSize'
        });
      }
    }
  };

  websocket.onopen = () => {
    console.log('[websockets] Connection opened!');
    status = 'connected_waiting_mode';
    loadingText = 'Connection established. Waiting for server mode...';
    updateStatusDisplay();
    window.postMessage({ type: 'trackpadModeUpdate', enabled: trackpadMode }, window.location.origin);
    if (!isSharedMode) {
      const settingsPrefix = `${storageAppName}_`;
      const settingsToSend = {};
      let foundSettings = false;
      let initialClientWidthForSettings, initialClientHeightForSettings;
      const dpr = useCssScaling ? 1 : (window.devicePixelRatio || 1);

      for (const key in localStorage) {
        if (Object.hasOwnProperty.call(localStorage, key) && key.startsWith(settingsPrefix)) {
          const unprefixedKey = key.substring(settingsPrefix.length);
          let serverExpectedKey = null;
          if (unprefixedKey === 'videoBitRate') serverExpectedKey = 'webrtc_videoBitRate';
          else if (unprefixedKey === 'videoFramerate') serverExpectedKey = 'webrtc_videoFramerate';
          else if (unprefixedKey === 'videoCRF') serverExpectedKey = 'webrtc_videoCRF';
          else if (unprefixedKey === 'encoder') serverExpectedKey = 'webrtc_encoder';
          else if (unprefixedKey === 'resizeRemote') serverExpectedKey = 'webrtc_resizeRemote';
          else if (unprefixedKey === 'isManualResolutionMode') serverExpectedKey = 'webrtc_isManualResolutionMode';
          else if (unprefixedKey === 'audioBitRate') serverExpectedKey = 'webrtc_audioBitRate';
          else if (unprefixedKey === 'videoBufferSize') serverExpectedKey = 'webrtc_videoBufferSize';
          else if (unprefixedKey === 'h264_fullcolor') serverExpectedKey = 'webrtc_h264_fullcolor';
          else if (unprefixedKey === 'h264_streaming_mode') serverExpectedKey = 'webrtc_h264_streaming_mode';
          else if (unprefixedKey === 'jpegQuality') serverExpectedKey = 'pixelflux_jpeg_quality';
          else if (unprefixedKey === 'paintOverJpegQuality') serverExpectedKey = 'pixelflux_paint_over_jpeg_quality';
          else if (unprefixedKey === 'useCpu') serverExpectedKey = 'pixelflux_use_cpu';
          else if (unprefixedKey === 'h264_paintover_crf') serverExpectedKey = 'pixelflux_h264_paintover_crf';
          else if (unprefixedKey === 'h264_paintover_burst_frames') serverExpectedKey = 'pixelflux_h264_paintover_burst_frames';
          else if (unprefixedKey === 'use_paint_over_quality') serverExpectedKey = 'pixelflux_use_paint_over_quality';
          else if (unprefixedKey === 'SCALING_DPI') serverExpectedKey = 'webrtc_SCALING_DPI';

          if (serverExpectedKey) {
            let value = localStorage.getItem(key);
            const booleanSettingKeys = [
              'webrtc_resizeRemote',
              'webrtc_isManualResolutionMode',
              'webrtc_h264_fullcolor',
              'webrtc_h264_streaming_mode',
              'pixelflux_use_cpu',
              'pixelflux_use_paint_over_quality',
            ];
            const integerSettingKeys = [
              'webrtc_videoBitRate',
              'webrtc_videoFramerate',
              'webrtc_videoCRF',
              'webrtc_audioBitRate',
              'webrtc_videoBufferSize',
              'pixelflux_jpeg_quality',
              'pixelflux_paint_over_jpeg_quality',
              'pixelflux_h264_paintover_crf',
              'pixelflux_h264_paintover_burst_frames',
              'webrtc_SCALING_DPI',
            ];
            if (booleanSettingKeys.includes(serverExpectedKey)) {
              value = (value === 'true');
            } else if (integerSettingKeys.includes(serverExpectedKey)) {
              value = parseInt(value, 10);
              if (isNaN(value)) value = localStorage.getItem(key);
            }
            settingsToSend[serverExpectedKey] = value;
            foundSettings = true;
          }
        }
      }

      if (isManualResolutionMode && manualWidth != null && manualHeight != null) {
        settingsToSend['webrtc_isManualResolutionMode'] = true;
        settingsToSend['webrtc_manualWidth'] = roundDownToEven(manualWidth * dpr);
        settingsToSend['webrtc_manualHeight'] = roundDownToEven(manualHeight * dpr);
      } else {
        const videoContainer = document.querySelector('.video-container');
        const rect = videoContainer ? videoContainer.getBoundingClientRect() : {
          width: window.innerWidth,
          height: window.innerHeight
        };
        initialClientWidthForSettings = rect.width;
        initialClientHeightForSettings = rect.height;

        settingsToSend['webrtc_isManualResolutionMode'] = false;
        settingsToSend['webrtc_initialClientWidth'] = roundDownToEven(initialClientWidthForSettings * dpr);
        settingsToSend['webrtc_initialClientHeight'] = roundDownToEven(initialClientHeightForSettings * dpr);
      }

      if (settingsToSend['webrtc_isManualResolutionMode'] === true) {
          const storedManualWidth = getIntParam('manualWidth', null);
          const storedManualHeight = getIntParam('manualHeight', null);
          if (storedManualWidth !== null && storedManualHeight !== null) {
              settingsToSend['webrtc_manualWidth'] = roundDownToEven(storedManualWidth * dpr);
              settingsToSend['webrtc_manualHeight'] = roundDownToEven(storedManualHeight * dpr);
          }
      }
      settingsToSend['webrtc_useCssScaling'] = useCssScaling;

      try {
        const settingsJson = JSON.stringify(settingsToSend);
        const message = `SETTINGS,${settingsJson}`;
        websocket.send(message);
        console.log('[websockets] Sent initial settings (resolutions are physical) to server:', settingsToSend);
      } catch (e) {
        console.error('[websockets] Error constructing or sending initial settings:', e);
      }

      const isCurrentModePixelfluxH264_ws = currentEncoderMode === 'x264enc' || currentEncoderMode === 'x264enc-striped';
      const isCurrentModeJpeg_ws = currentEncoderMode === 'jpeg';
      const isCurrentModeGStreamerPipeline_ws = !isCurrentModePixelfluxH264_ws && !isCurrentModeJpeg_ws;

      if (isCurrentModeGStreamerPipeline_ws) {
        if (websocket && websocket.readyState === WebSocket.OPEN) {
          const bitrateMessage = `SET_VIDEO_BITRATE,${videoBitRate}`;
          websocket.send(bitrateMessage);
          console.log(`[websockets] Sent initial SET_VIDEO_BITRATE,${videoBitRate} for GStreamer encoder.`);
        }
      }

    } else {
        console.log("Shared mode: WebSocket opened. Waiting for 'MODE websockets' from server to start identification sequence.");
    }

    websocket.send('cr');
    console.log('[websockets] Sent initial clipboard request (cr) to server.');

    isVideoPipelineActive = true;
    isAudioPipelineActive = true;
    window.postMessage({
      type: 'pipelineStatusUpdate',
      video: true,
      audio: true
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

    if (heartbeatIntervalId === null) {
      heartbeatIntervalId = setInterval(sendHeartbeat, HEARTBEAT_INTERVAL_MS);
      console.log(`[websockets] Started heartbeat every ${HEARTBEAT_INTERVAL_MS}ms to prevent connection timeouts.`);
    }
  };

  websocket.onmessage = (event) => {
    if (event.data instanceof ArrayBuffer) {
      const arrayBuffer = event.data;
      const dataView = new DataView(arrayBuffer);
      if (arrayBuffer.byteLength < 1) return;
      const dataTypeByte = dataView.getUint8(0);

      if (isSharedMode) {
        if (sharedClientState === 'awaiting_identification') {
            let identifiedType = null;
            if (dataTypeByte === 0) identifiedType = 'h264_full_frame';
            else if (dataTypeByte === 0x03) identifiedType = 'jpeg';
            else if (dataTypeByte === 0x04) identifiedType = 'x264enc-striped';

            if (identifiedType) {
                clearSharedModeProbingTimeout();
                sharedProbingAttempts = 0;
                identifiedEncoderModeForShared = identifiedType;
                console.log(`Shared mode: Identified video encoding type as '${identifiedEncoderModeForShared}' from first packet (type 0x${dataTypeByte.toString(16)}). State: configuring.`);
                sharedClientState = 'configuring';

                console.log("Shared mode: Cleaning up existing video pipeline elements for reconfiguration.");
                if (decoder && decoder.state !== 'closed') {
                    try { decoder.close(); } catch (e) { console.warn("Shared mode: Error closing main H.264 decoder during cleanup:", e); }
                    decoder = null;
                }
                clearAllVncStripeDecoders();
                cleanupVideoBuffer();
                cleanupJpegStripeQueue();
                decodedStripesQueue = [];

                if (canvasContext && canvas) {
                    console.log("Shared mode: Resetting canvas display.");
                    canvasContext.setTransform(1, 0, 0, 1, 0, 0);
                    canvasContext.clearRect(0, 0, canvas.width, canvas.height);
                }

                currentEncoderMode = identifiedEncoderModeForShared;
                console.log(`Shared mode: Set global currentEncoderMode to '${currentEncoderMode}'.`);

                if (identifiedEncoderModeForShared === 'h264_full_frame') {
                    console.log("Shared mode: Initializing main H.264 decoder for the identified type.");
                    triggerInitializeDecoder().then(success => { // Uses logical manualWidth/Height, applies DPR
                        if (success) {
                            console.log("Shared mode: H.264 decoder configured. Requesting fresh video stream.");
                            sharedClientState = 'ready';
                            console.log(`Shared mode: Client is now ready to process video of type '${identifiedEncoderModeForShared}'.`);
                        } else {
                            console.error("Shared mode: Main H.264 decoder failed to initialize or configure. Entering error state.");
                            sharedClientState = 'error';
                        }
                    }).catch(initError => {
                        console.error("Shared mode: Exception during H.264 decoder initialization. Entering error state.", initError);
                        sharedClientState = 'error';
                    });
                } else if (identifiedEncoderModeForShared === 'jpeg' || identifiedEncoderModeForShared === 'x264enc-striped') {
                    console.log(`Shared mode: Configured for ${identifiedEncoderModeForShared}. Specific decoders (if any) are managed on-demand or not needed centrally.`);
                    if (manualWidth && manualHeight && manualWidth > 0 && manualHeight > 0) { // manualWidth/Height are logical
                         applyManualCanvasStyle(manualWidth, manualHeight, true); // Handles DPR
                    }
                    console.log("Shared mode: Reconfiguration process for non-H264 initiated. Requesting fresh video stream from server.");
                    sharedClientState = 'ready';
                    console.log(`Shared mode: Client is now ready to process video of type '${identifiedEncoderModeForShared}'.`);
                }
            } else if (dataTypeByte !== 1) { // Ignore audio packets during identification
                console.warn(`Shared mode (awaiting_identification): Received non-identifying binary packet type 0x${dataTypeByte.toString(16)}. Still waiting for a video packet.`);
                return;
            }
        } else if (sharedClientState === 'ready') {
            let packetIsVideo = (dataTypeByte === 0 || dataTypeByte === 0x03 || dataTypeByte === 0x04);
            if (packetIsVideo) {
                let packetMatchesIdentifiedType = false;
                if (identifiedEncoderModeForShared === 'h264_full_frame' && dataTypeByte === 0) packetMatchesIdentifiedType = true;
                else if (identifiedEncoderModeForShared === 'jpeg' && dataTypeByte === 0x03) packetMatchesIdentifiedType = true;
                else if (identifiedEncoderModeForShared === 'x264enc-striped' && dataTypeByte === 0x04) packetMatchesIdentifiedType = true;

                if (!packetMatchesIdentifiedType) {
                    console.warn(`Shared mode (ready): Received video packet type 0x${dataTypeByte.toString(16)} which does NOT match identified type '${identifiedEncoderModeForShared}'. Discarding packet.`);
                    return;
                }
            }
        } else if (sharedClientState === 'configuring' || sharedClientState === 'error' || sharedClientState === 'idle') {
            let packetIsVideo = (dataTypeByte === 0 || dataTypeByte === 0x03 || dataTypeByte === 0x04);
            if (packetIsVideo) {
                 console.log(`Shared mode: Video packet (type 0x${dataTypeByte.toString(16)}) received while in state '${sharedClientState}'. Discarding.`);
                 return;
            }
        }
      }


      if (dataTypeByte === 0) {
        const headerLength = isSharedMode ? 2 : 4;
        if (arrayBuffer.byteLength < headerLength) return;

        const frameTypeFlag = dataView.getUint8(1);
        if (!isSharedMode) lastReceivedVideoFrameId = dataView.getUint16(2, false);
        const videoDataArrayBuffer = arrayBuffer.slice(headerLength);

        const canProcessFullH264 =
          (isSharedMode && sharedClientState === 'ready' && currentEncoderMode === 'h264_full_frame') ||
          (!isSharedMode && isVideoPipelineActive && currentEncoderMode !== 'jpeg' && currentEncoderMode !== 'x264enc' && currentEncoderMode !== 'x264enc-striped');

        if (canProcessFullH264) {
          if (isSharedMode && !sharedClientHasReceivedKeyframe) {
            if (frameTypeFlag === 1) {
              console.log("Shared mode: First keyframe received. Opening the gate for video decoding.");
              sharedClientHasReceivedKeyframe = true;
            } else {
              console.log("Shared mode: Gate is closed. Discarding non-keyframe packet.");
              return;
            }
          }
          if (decoder && decoder.state === 'configured') {
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
              initializeDecoder(); // Uses logical dimensions, applies DPR
            } else if (isSharedMode && (!decoder || decoder.state === 'closed' || decoder.state === 'unconfigured')) {
                 console.error(`Shared mode: Main H.264 decoder not available or not configured when expected. State: ${sharedClientState}. Decoder state: ${decoder ? decoder.state : 'null'}. Entering error state.`);
                 sharedClientState = 'error';
            } else {
              console.warn(`Main decoder exists but not configured (state: ${decoder.state}). Full H.264 frame dropped.`);
            }
          }
        }


      } else if (dataTypeByte === 1) {
        const audioHeaderLength = 2;
        if (arrayBuffer.byteLength < audioHeaderLength) return;

        if ((isAudioPipelineActive || isSharedMode)) {
          if (audioDecoderWorker) {
            if (audioContext && audioContext.state !== 'running') {
              audioContext.resume().catch(e => console.error("Error resuming audio context", e));
            }
            const opusDataArrayBuffer = arrayBuffer.slice(audioHeaderLength);
            if (opusDataArrayBuffer.byteLength > 0) {
              if (!isSharedMode && window.currentAudioBufferSize >= 5) {
                return;
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
                const opusDataArrayBuffer = arrayBuffer.slice(audioHeaderLength);
                if (opusDataArrayBuffer.byteLength > 0) {
                  if (!isSharedMode && window.currentAudioBufferSize >= 5) return;
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

        if (!isSharedMode) lastReceivedVideoFrameId = dataView.getUint16(2, false);
        const stripe_y_start = dataView.getUint16(isSharedMode ? 2 : 4, false);
        const jpegDataBuffer = arrayBuffer.slice(jpegHeaderLength);

        const canProcessJpeg =
          (isSharedMode && sharedClientState === 'ready' && currentEncoderMode === 'jpeg') ||
          (!isSharedMode && isVideoPipelineActive && currentEncoderMode === 'jpeg');

        if (canProcessJpeg) {
          if (jpegDataBuffer.byteLength === 0) return;
          decodeAndQueueJpegStripe(stripe_y_start, jpegDataBuffer);
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
        const stripeWidth = dataView.getUint16(6, false); // Physical
        const stripeHeight = dataView.getUint16(8, false); // Physical
        const h264Payload = arrayBuffer.slice(EXPECTED_HEADER_LENGTH);

        const canProcessVncStripe =
            (isSharedMode && sharedClientState === 'ready' && (currentEncoderMode === 'x264enc' || currentEncoderMode === 'x264enc-striped')) ||
            (!isSharedMode && isVideoPipelineActive && (currentEncoderMode === 'x264enc' || currentEncoderMode === 'x264enc-striped'));

        if (canProcessVncStripe) {
            if (isSharedMode && !sharedClientHasReceivedKeyframe) {
                if (video_frame_type_byte === 0x01) {
                    console.log("Shared mode: First keyframe received for striped video. Opening the gate.");
                    sharedClientHasReceivedKeyframe = true;
                } else {
                    console.log("Shared mode: Gate is closed. Discarding non-keyframe striped packet.");
                    return;
                }
            }
            if (h264Payload.byteLength === 0) return;

            let decoderInfo = vncStripeDecoders[vncStripeYStart];
            const chunkType = (video_frame_type_byte === 0x01) ? 'key' : 'delta';
            if (!decoderInfo || decoderInfo.decoder.state === 'closed' ||
                (decoderInfo.decoder.state === 'configured' && (decoderInfo.width !== stripeWidth || decoderInfo.height !== stripeHeight))) {

                if(decoderInfo && decoderInfo.decoder.state !== 'closed') {
                    try { decoderInfo.decoder.close(); } catch(e) { console.warn("Error closing old VNC stripe decoder:", e); }
                }

                const newStripeDecoder = new VideoDecoder({
                    output: handleDecodedVncStripeFrame.bind(null, vncStripeYStart, vncFrameID),
                    error: (e) => initiateFallback(e, `stripe_decoder_Y=${vncStripeYStart}`)
                });
                const decoderConfig = { // Configured with physical dimensions
                    codec: 'avc1.42E01E',
                    codedWidth: stripeWidth,
                    codedHeight: stripeHeight,
                    optimizeForLatency: true
                };
                vncStripeDecoders[vncStripeYStart] = {
                    decoder: newStripeDecoder,
                    pendingChunks: [],
                    width: stripeWidth, // Store physical dimensions used for this decoder
                    height: stripeHeight
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
      if (event.data === 'MODE websockets') {
        clientMode = 'websockets';
        console.log('[websockets] Switched to websockets mode.');
        status = 'initializing';
        loadingText = 'Initializing WebSocket mode...';
        updateStatusDisplay();

        if (decoder && decoder.state !== "closed") {
            try { decoder.close(); } catch(e){}
            decoder = null;
        }
        clearAllVncStripeDecoders();
        cleanupVideoBuffer();
        cleanupJpegStripeQueue();
        decodedStripesQueue = [];

        if (!isSharedMode) {
            stopMicrophoneCapture();
            if (currentEncoderMode !== 'jpeg' && currentEncoderMode !== 'x264enc' && currentEncoderMode !== 'x264enc-striped') {
              initializeDecoder(); // Uses logical dimensions, applies DPR
            }
        }

        initializeAudio().then(() => {
          initializeDecoderAudio();
        });

        initializeInput(); // Sets up canvas based on logical dimensions, handles DPR

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
            sharedClientState = 'awaiting_identification';
            sharedProbingAttempts = 0;
            identifiedEncoderModeForShared = null;
            console.log("Shared mode: Received 'MODE websockets'. Requesting initial stream with STOP/START_VIDEO. State: awaiting_identification.");
            if (websocket && websocket.readyState === WebSocket.OPEN) {
                 websocket.send('STOP_VIDEO');
                 setTimeout(() => {
                    if (websocket && websocket.readyState === WebSocket.OPEN) {
                        websocket.send('START_VIDEO');
                        console.log("Shared mode: Sent START_VIDEO after initial STOP_VIDEO.");
                    }
                }, 250);
            }
            startSharedModeProbingTimeout();
        } else {
            if (websocket && websocket.readyState === WebSocket.OPEN) {
              if (!document.hidden && isVideoPipelineActive) websocket.send('START_VIDEO');
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
          else if (obj.type === 'server_settings') window.postMessage({
            type: 'serverSettings',
            encoders: obj.encoders
          }, window.location.origin);
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
              if (!isVideoPipelineActive && (currentEncoderMode === 'x264enc' || currentEncoderMode === 'x264enc-striped') && !isSharedMode) {
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
         } else if (obj.type === 'stream_resolution') { // Server sends physical dimensions
           const dpr_for_conversion = useCssScaling ? 1 : (window.devicePixelRatio || 1);
           if (isSharedMode) {
             if (sharedClientState === 'error' || sharedClientState === 'idle') {
                 console.log(`Shared mode: Received stream_resolution while in state '${sharedClientState}'. Ignoring.`);
             } else {
                 const physicalNewWidth = parseInt(obj.width, 10);
                 const physicalNewHeight = parseInt(obj.height, 10);

                 if (physicalNewWidth > 0 && physicalNewHeight > 0) {
                     const evenPhysicalNewWidth = roundDownToEven(physicalNewWidth);
                     const evenPhysicalNewHeight = roundDownToEven(physicalNewHeight);

                     // Convert to logical for storage and comparison with logical manualWidth/Height
                     const logicalNewWidth = evenPhysicalNewWidth / dpr_for_conversion;
                     const logicalNewHeight = evenPhysicalNewHeight / dpr_for_conversion;

                     let dimensionsChanged = (manualWidth !== logicalNewWidth || manualHeight !== logicalNewHeight);

                     if (dimensionsChanged) {
                         console.log(`Shared mode: Received stream_resolution from server: ${physicalNewWidth}x${physicalNewHeight} (physical, rounded to ${evenPhysicalNewWidth}x${evenPhysicalNewHeight}). Current manual (logical): ${manualWidth ? manualWidth.toFixed(2):'null'}x${manualHeight ? manualHeight.toFixed(2):'null'}. New logical: ${logicalNewWidth.toFixed(2)}x${logicalNewHeight.toFixed(2)}. Current state: ${sharedClientState}.`);
                         manualWidth = logicalNewWidth; // Store as logical
                         manualHeight = logicalNewHeight; // Store as logical
                         applyManualCanvasStyle(manualWidth, manualHeight, true); // Takes logical, handles DPR
                     }

                     if (sharedClientState === 'ready' && dimensionsChanged && identifiedEncoderModeForShared === 'h264_full_frame') {
                         console.log(`Shared mode (stream_resolution, ready state): Identified mode is h264_full_frame. Triggering main decoder re-init for new logical resolution ${manualWidth.toFixed(2)}x${manualHeight.toFixed(2)}.`);
                         triggerInitializeDecoder(); // Uses global logical manualWidth/Height, applies DPR
                     } else if (sharedClientState === 'ready' && dimensionsChanged && (identifiedEncoderModeForShared === 'x264enc-striped' || identifiedEncoderModeForShared === 'jpeg')) {
                         console.log(`Shared mode (stream_resolution, ready state): Mode is ${identifiedEncoderModeForShared}. Clearing canvas due to base resolution change.`);
                         if (canvasContext && canvas.width > 0 && canvas.height > 0) {
                             canvasContext.setTransform(1, 0, 0, 1, 0, 0);
                             canvasContext.clearRect(0, 0, canvas.width, canvas.height);
                         }
                     }
                 } else {
                     console.warn(`Shared mode: Received invalid stream_resolution dimensions: ${obj.width}x${obj.height}`);
                 }
             }
           } else {
             console.log(`Non-shared mode: Received stream_resolution (ignored for control): ${obj.width}x${obj.height}`);
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
            navigator.clipboard.writeText(decodedText).catch(err => console.error('Could not copy server clipboard to local: ' + err));
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
          isVideoPipelineActive = true;
          window.postMessage({ type: 'pipelineStatusUpdate', video: true }, window.location.origin);
        }
        else if (event.data === 'VIDEO_STOPPED' && !isSharedMode) {
          console.log("Client: Received VIDEO_STOPPED. Updating isVideoPipelineActive=false. Expecting PIPELINE_RESETTING from server for full state reset.");
          isVideoPipelineActive = false;
          window.postMessage({ type: 'pipelineStatusUpdate', video: false }, window.location.origin);
        } else if (event.data === 'PONG') {
          console.log('[Heartbeat] Received PONG from server');
        }
        else if (event.data.startsWith('PIPELINE_RESETTING ')) {
          const parts = event.data.split(' ');
          const newEpochStartId = parts.length > 1 ? parseInt(parts[1], 10) : 0; // Usually 0
          console.log(`[websockets] Received PIPELINE_RESETTING. New epoch start ID: ${newEpochStartId}. Current lastReceivedVideoFrameId: ${lastReceivedVideoFrameId}`);

          performServerInitiatedVideoReset(`PIPELINE_RESETTING from server, new epoch ${newEpochStartId}`);
          if (isSharedMode) {
            console.log(`Shared mode: PIPELINE_RESETTING received. Current state: ${sharedClientState}, Identified encoder: ${identifiedEncoderModeForShared}`);
            sharedClientState = 'awaiting_identification';
            clearSharedModeProbingTimeout();
            identifiedEncoderModeForShared = null;
            sharedProbingAttempts = 0;
            console.log("Shared mode: Transitioned to 'awaiting_identification'. Passively waiting for new video data.");
            startSharedModeProbingTimeout();
          } else {
            console.log("Non-shared mode: Video reset complete. Decoder (if applicable) will be re-initialized if pipeline is active.");
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
        } else {
          if (window.webrtcInput && window.webrtcInput.on_message && !isSharedMode) {
            window.webrtcInput.on_message(event.data);
          }
        }
      }
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
    if (heartbeatIntervalId) {
      clearInterval(heartbeatIntervalId);
      heartbeatIntervalId = null;
    }
    releaseWakeLock();
    if (isSharedMode) {
        console.error("Shared mode: WebSocket error. Resetting shared state to 'error'.");
        sharedClientState = 'error';
        clearSharedModeProbingTimeout();
        sharedProbingAttempts = 0;
    }
  };

  websocket.onclose = (event) => {
    console.log('[websockets] Connection closed', event);
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
    if (heartbeatIntervalId) {
      clearInterval(heartbeatIntervalId);
      heartbeatIntervalId = null;
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
        clearSharedModeProbingTimeout();
        sharedProbingAttempts = 0;
        identifiedEncoderModeForShared = null;
    }
  };

  setInterval(() => {
    if (clientMode === 'websockets' && websocket && websocket.readyState === WebSocket.OPEN) {
      // Connection is fine
    } else {
      console.log("WebSocket not open or not in WebSocket mode, reloading page to reconnect.");
      location.reload();
    }
  }, 3000);
});

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
    // In shared mode, pipelineActive is effectively always true from worker's perspective for processing
    // if (!pipelineActive) {
    //   try { frame.close(); } catch(e) { /* ignore */ }
    //   return;
    // }
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
        await initializeDecoderInWorker();
        break;
      case 'decode':
        // if (!pipelineActive) return; // Allow decode even if main thread says inactive, for shared mode to always process
        if (decoderAudio && decoderAudio.state === 'configured') {
          const chunk = new EncodedAudioChunk({ type: 'key', timestamp: data.timestamp || (performance.now() * 1000), data: data.opusBuffer });
          try {
            if (currentDecodeQueueSize < 20) { // Limit queue to prevent OOM with bad data
                 decoderAudio.decode(chunk); currentDecodeQueueSize++;
            } else {
                // console.warn('[AudioWorker] Decode queue full, dropping audio chunk.');
            }
          } catch (e) {
              currentDecodeQueueSize = Math.max(0, currentDecodeQueueSize - 1);
              if (decoderAudio.state === 'closed' || decoderAudio.state === 'unconfigured') await initializeDecoderInWorker();
          }
        } else if (!decoderAudio || (decoderAudio && decoderAudio.state !== 'configuring')) {
          await initializeDecoderInWorker(); // Try to reinit if not configured
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
    micWorkletNode.port.onmessage = (event) => {
      const pcm16Buffer = event.data;
      if (websocket && websocket.readyState === WebSocket.OPEN && isMicrophoneActive) {
        if (!pcm16Buffer || !(pcm16Buffer instanceof ArrayBuffer) || pcm16Buffer.byteLength === 0) return;
        const messageBuffer = new ArrayBuffer(1 + pcm16Buffer.byteLength);
        const messageView = new DataView(messageBuffer);
        messageView.setUint8(0, 0x02);
        new Uint8Array(messageBuffer, 1).set(new Uint8Array(pcm16Buffer));
        try {
          websocket.send(messageBuffer);
        } catch (e) {
          console.error("Error sending mic data:", e);
        }
      }
    };
    micWorkletNode.port.onmessageerror = (event) => console.error("Error from mic worklet:", event);
    micSourceNode.connect(micWorkletNode);
    isMicrophoneActive = true;
    postSidebarButtonUpdate();
  } catch (error) {
    console.error('Failed to start microphone capture:', error);
    alert(`Microphone error: ${error.name} - ${error.message}`);
    stopMicrophoneCapture(); // This will set isMicrophoneActive = false and update UI
  }
}

function stopMicrophoneCapture() {
  // This function is safe to call even in shared mode, it will just do nothing if mic wasn't active.
  if (!isMicrophoneActive && !micStream && !micAudioContext) {
    if (isMicrophoneActive) { // Should not happen if first condition is true, but defensive
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
  if (isMicrophoneActive) { // Only update if it was active
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
  if (heartbeatIntervalId) {
    clearInterval(heartbeatIntervalId);
    heartbeatIntervalId = null;
  }
  releaseWakeLock();
  if (window.isCleaningUp) return;
  window.isCleaningUp = true;
  console.log("Cleanup: Starting cleanup process...");
  if (!isSharedMode) stopMicrophoneCapture(); // Microphone only for non-shared

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
      audioDecoderWorker.postMessage({
        type: 'close'
      }); // Worker will terminate itself
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
  inputInitialized = false; // Reset input initialization flag
  if (statusDisplayElement) statusDisplayElement.textContent = 'Connecting...';
  if (statusDisplayElement) statusDisplayElement.classList.remove('hidden');
  if (playButtonElement) playButtonElement.classList.remove('hidden');
  if (overlayInput) overlayInput.style.cursor = 'auto';
  serverClipboardContent = '';
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
  if (isSharedMode) { // Prevent drop indication in shared mode
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
    if (websocket && websocket.readyState === WebSocket.OPEN) websocket.send(`FILE_UPLOAD_ERROR:GENERAL:Processing failed`);
  }
}

function getFileFromEntry(fileEntry) {
  return new Promise((resolve, reject) => fileEntry.file(resolve, reject));
}

async function handleDroppedEntry(entry, basePathFallback = "") { // basePathFallback is for non-fullPath scenarios
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
      const file = await getFileFromEntry(entry); // Assume getFileFromEntry is defined
      await uploadFileObject(file, pathToSend);
    } catch (err) {
      console.error(`Error processing file ${pathToSend}: ${err}`);
       window.postMessage({
        type: 'fileUpload',
        payload: { status: 'error', fileName: pathToSend, message: `Error processing file: ${err.message || err}` }
      }, window.location.origin);
      if (websocket && websocket.readyState === WebSocket.OPEN) {
         websocket.send(`FILE_UPLOAD_ERROR:${pathToSend}:Client-side file processing error`);
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
    websocket.send(`FILE_UPLOAD_START:${pathToSend}:${file.size}`);
    let offset = 0;
    fileUploadProgressLastSent[pathToSend] = 0;
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
        websocket.send(`FILE_UPLOAD_ERROR:${pathToSend}:File read error`);
        reject(e.target.error);
        return;
      }
      try {
        const prefixedView = new Uint8Array(1 + e.target.result.byteLength);
        prefixedView[0] = 0x01; // Data prefix for file chunk
        prefixedView.set(new Uint8Array(e.target.result), 1);
        websocket.send(prefixedView.buffer);
        offset += e.target.result.byteLength;
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
          setTimeout(() => readChunk(offset), 0);
        } else {
          window.postMessage({
            type: 'fileUpload',
            payload: { status: 'progress', fileName: pathToSend, progress: 100, fileSize: file.size }
          }, window.location.origin);
          websocket.send(`FILE_UPLOAD_END:${pathToSend}`);
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
        websocket.send(`FILE_UPLOAD_ERROR:${pathToSend}:WS send error`);
        reject(wsError);
      }
    };
    reader.onerror = function(e) {
      const generalReadError = `General file reader error for ${pathToSend}: ${e.target.error}`;
      window.postMessage({ type: 'fileUpload', payload: { status: 'error', fileName: pathToSend, message: generalReadError }}, window.location.origin);
      websocket.send(`FILE_UPLOAD_ERROR:${pathToSend}:General file reader error`);
      reject(e.target.error);
    };

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
    console.log("  Shared mode reset: Gate closed. Waiting for a new keyframe.");
  }

  lastReceivedVideoFrameId = -1;
  console.log(`  Reset lastReceivedVideoFrameId to ${lastReceivedVideoFrameId}.`);

  cleanupVideoBuffer();
  cleanupJpegStripeQueue();
  decodedStripesQueue = [];

  if (currentEncoderMode === 'x264enc' || currentEncoderMode === 'x264enc-striped') {
    clearAllVncStripeDecoders();
  } else if (currentEncoderMode !== 'jpeg') {
    if (decoder && decoder.state !== 'closed') {
      console.log("  Closing main video decoder due to server reset.");
      try { decoder.close(); } catch(e) { console.warn("  Error closing main video decoder during reset:", e); }
    }
    decoder = null;
    console.log("  Main video decoder instance set to null.");
  }

  if (canvasContext && canvas && !(currentEncoderMode === 'x264enc' || currentEncoderMode === 'x264enc-striped')) {
    try {
      canvasContext.setTransform(1, 0, 0, 1, 0, 0);
      canvasContext.clearRect(0, 0, canvas.width, canvas.height);
      console.log("  Cleared canvas during server-initiated reset.");
    } catch (e) {
      console.error("  Error clearing canvas during server-initiated reset:", e);
    }
  }

  if (!isSharedMode) {
    if (currentEncoderMode !== 'jpeg' && currentEncoderMode !== 'x264enc' && currentEncoderMode !== 'x264enc-striped') {
      console.log("  Ensuring main video decoder is re-initialized after server reset.");
      if (isVideoPipelineActive) {
         triggerInitializeDecoder();
      } else {
        console.log("  isVideoPipelineActive is false, decoder re-initialization deferred until video is enabled by user.");
      }
    }
  }
}

function initiateFallback(error, context) {
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
        setStringParam('encoder', 'x264enc');
        setBoolParam('h264_fullcolor', false);
        setIntParam('videoFramerate', 60);
        setIntParam('videoCRF', 25);
        setBoolParam('isManualResolutionMode', false);
        setIntParam('manualWidth', null);
        setIntParam('manualHeight', null);
        
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
