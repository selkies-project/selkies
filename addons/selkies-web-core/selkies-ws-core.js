/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * WebSocket streaming core: the page-side half of the WebSocket transport,
 * started by selkies-core.js when the stored stream mode is `websockets`.
 *
 * One socket at `<route prefix>/api/websockets` carries the whole session. It
 * is read in a worker (`SOCKET_WORKER_SRC`, reached through
 * `WorkerWebSocket`), which passes audio straight to the decoder and the
 * playback worklet, routes video straight to the video worker while it
 * decodes -- full frames through one decoder, the striped modes decoded per
 * row and composited there, acked from the socket worker (received id for
 * full frames, presented id for stripes) -- and carries the microphone and
 * webcam encoders' frames out over their own ports: nothing occupying the
 * page's thread can interrupt any of them.
 * Binary messages are typed by their first byte. From the server: `0x01`
 * audio (Opus, with the RED redundancy layout documented on
 * extractOpusFrames), `0x03` a JPEG stripe (`u8 reserved`, `u16 frame id`,
 * `u16 stripe Y`, JPEG data), `0x04` an H.264 stripe or full frame (`u8
 * keyframe`, `u16 frame id`, `u16 stripe Y`, `u16 width`, `u16 height`,
 * Annex-B data), and `0x05` a gzip-wrapped control text once the client
 * advertised `_gz,1`. From the
 * client: `0x02` microphone Opus, `0x06` webcam frames (startWebcamCapture),
 * and `0x05` gzipped large text once the server echoed `_gz,1`. Text messages
 * are control. The client sends `SETTINGS,{json}`, `r,WxH,displayId`,
 * `START_VIDEO`, `STOP_VIDEO`, `START_AUDIO`, `STOP_AUDIO`,
 * `REQUEST_KEYFRAME`, `CLIENT_FRAME_ACK <id>`, `cr`, `REQUEST_CLIPBOARD`, the
 * chunked clipboard upload of lib/clipboard-worker-bridge.js,
 * `cmd,<command>`, `SET_NATIVE_CURSOR_RENDERING,<0|1>`,
 * `vp,<originX>,<originY>,<scaleX>,<scaleY>` (this page's stream box on the
 * user's desktop, relayed to the other displays) and the input verbs of
 * lib/input.js. The server sends `MODE websockets`, `AUTH_SUCCESS,{json}`,
 * `ROLE_UPDATE,{json}`, `MK_ACCESS,<0|1>`, `VIDEO_STARTED`, `VIDEO_STOPPED`,
 * `AUDIO_STARTED`, `AUDIO_STOPPED`, `AUDIO_DISABLED`, `MICROPHONE_DISABLED`,
 * `WEBCAM_DISABLED`, `WEBCAM_KEYFRAME`, `PIPELINE_RESETTING <display>`,
 * `DISPLAY_CONFIG_UPDATE,{json}`, `cursor,{json}`, `system,{json}`,
 * `KILL <reason>`, the clipboard family (`clipboard,`, `clipboard_binary,`,
 * `clipboard_start,`, `clipboard_data,`, `clipboard_finish`,
 * `clipboard_reply,`), and JSON objects typed `server_settings`,
 * `server_apps`, `pipeline_status`, `stream_resolution`, `system_stats`,
 * `gpu_stats` and `network_stats`.
 *
 * Video is decoded with WebCodecs: a JPEG stripe through ImageDecoder, an
 * H.264 stripe through a VideoDecoder per row offset -- in the video worker
 * while the divert holds, on the page as the fallback -- and a full frame --
 * controller or shared viewer alike -- in the video worker or through the
 * row-0 stripe decoder. Decoded frames reach the
 * screen through the first sink available: a track generator feeding a
 * `<video>`, the worker's OffscreenCanvas, or the page canvas, with the
 * striped modes composited on a back-buffer and blitted whole at frame
 * boundaries. Audio is decoded in a worker and played through an
 * AudioWorklet, the microphone is encoded to Opus in a worker, and the webcam
 * is lib/webcam-capture.js.
 *
 * Dashboards talk to the core over same-origin window messages. The core
 * handles `setVolume`, `setMute`, `setScaleLocally`, `setSynth`,
 * `showVirtualKeyboard`, `setUseCssScaling`, `setAntiAliasing`,
 * `setUseBrowserCursors`, `setManualResolution`, `resetResolutionToWindow`,
 * `settings`, `getStats`, `clipboardUpdateFromUI`, `clipboardImageUpdate`,
 * `pipelineStatusUpdate`, `pipelineControl`, `audioDeviceSelected`,
 * `gamepadControl`, `requestFullscreen`, `command`, `touchinput:trackpad`,
 * `touchinput:touch` and `sidebarVisibilityChanged`, and posts
 * `pipelineStatusUpdate`, `sidebarButtonStatusUpdate`, `serverSettings`,
 * `systemApps`, `stats` (to the parent window), `clientRoleUpdate`,
 * `effectiveCursorState`, `trackpadModeUpdate`, `clipboardContentUpdate`,
 * the clipboard preview of lib/clipboard-sync.js, `fileUpload`,
 * `toggleDashboard` and `toggleTouchGamepad`. The `window` globals it
 * publishes for the dashboards and the tests are `webrtcInput` (the Input
 * handler), `fps`, `videoChunksReceived`, `videoDivertOn`, `videoStripeRows`
 * (the row layout the video worker is decoding), `webcamCodec`,
 * `system_stats`, `gpu_stats`,
 * `network_stats`, `currentAudioBufferSize`,
 * `currentAudioBufferDuration`, `currentAudioLevel`,
 * `currentAudioUnderrunSamples`, `currentAudioWorkletDropped`,
 * `currentAudioDropped`, `manual_resolution`, `enable_resize`,
 * `streamResolutionDiverged`, `isAudioInitializing`, `isFallingBack`,
 * `isCleaningUp`, `applyTimestamp` and `selkiesTransport` (the page-side
 * handle on the session socket, which itself runs in a worker), plus one
 * `window[key]` per server setting mirrored by sanitizeAndStoreSettings.
 *
 * Settings are read from localStorage at init with fallbacks only and persist
 * nothing, so a fresh profile keeps every key unset and server-pushed
 * defaults stay re-pushable; only genuine user actions, and
 * sanitizeAndStoreSettings for keys the user already overrode, write
 * localStorage. Keys in `PER_DISPLAY_SETTINGS` carry a `_display2` suffix on
 * the secondary display.
 * @module
 */

import {
  GamepadManager
} from './lib/gamepad.js';
import {
  Input
} from './lib/input.js';
import {
  createClipboardSync,
  createClipboardGestures,
  createLocalClipboardSender,
  createMultipartClipboardState,
  createTaggedClipboardFetch,
  writeImageToLocalClipboard,
  localClipboardBlocker,
  createDeferredClipboardWriter,
  clipboardPreviewMessage,
  digestedPayload
} from './lib/clipboard-sync.js';
import { ClipboardWorkerBridge, sendClipboardChunked } from './lib/clipboard-worker-bridge.js';
import {
  createFileUploader
} from './lib/file-upload.js';
import { detectKeyboardLayout } from './lib/keyboard-layout.js';
import { installAuthGuard } from './lib/auth-guard.js';
import { installSessionCookie, sessionAuthHeaders } from './lib/session-token.js';
import { storageKeyForServerKey, resolveSpec, HIDPI_SPEC } from './lib/conditional-settings.js';
import { getRoutePrefix, getStorageAppName, canDecodeEncoder, canDecodeFullColor } from './lib/util.js';
import { createStripeClock } from './lib/stripe-clock.js';
import { WebcamCapture, WEBCAM_ENCODER_PREFERENCES } from './lib/webcam-capture.js';

installAuthGuard();
installSessionCookie();

/**
 * Best-effort local keyboard layout, `null` while unknown (and then omitted from
 * settings). Resolved once at script init so it is ready by the time the socket
 * connects; a probe that lands after the initial SETTINGS payload sends the
 * hint on its own, guarded because the connection may not exist yet, and never
 * from a shared viewer, which pushes no settings.
 */
let detectedKeyboardLayout = null;
detectKeyboardLayout().then((layout) => {
    detectedKeyboardLayout = layout;
    try {
        if (layout && !isSharedMode && typeof websocket !== 'undefined' && websocket &&
            websocket.readyState === WebSocket.OPEN) {
            websocket.send(`SETTINGS,${JSON.stringify({ keyboardLayout: layout })}`);
        }
    } catch (e) { /* pre-connect */ }
});

/** Timestamp of the newest Opus frame handed to the decoder; `null` before RED starts. */
let lastAudioTs = null;
/**
 * 32-bit wrap-safe comparison of audio timestamps.
 * @param {number} a
 * @param {number} b
 * @returns {boolean} True when `a` is strictly newer than `b`.
 */
function audioTsNewer(a, b) {
  const d = (a - b) >>> 0;
  return d !== 0 && d < 0x80000000;
}
/**
 * Parses an audio message body into the ordered Opus frames to decode, using
 * RED redundancy to recover frames the sender dropped under backpressure
 * (pcmflux's delivery ring and the server's audio queue both drop-oldest, and
 * a dropped frame rides along as redundancy in the next packet).
 *
 * `n_red == 0` is the plain path: `[0x01, 0x00] + opus`. `n_red > 0` is
 * `[0x01, n_red, pts32] + n_red * (4-byte header) + 1-byte primary header +
 * block data`, redundant blocks oldest-first and then the primary; each block's
 * timestamp is `pts - tsOffset`. Every frame is decoded at most once, in
 * order: any block newer than the last one already played is taken, so a
 * redundant copy fills the gap left by a dropped primary. The first RED packet
 * anchors on its primary without replaying its redundancy.
 * @param {ArrayBuffer} arrayBuffer The whole binary message, type byte included.
 * @returns {ArrayBuffer[]} Opus frames in decode order; empty for a malformed packet.
 */
function extractOpusFrames(arrayBuffer) {
  const bytes = new Uint8Array(arrayBuffer);
  const nRed = bytes[1];
  if (!nRed) { lastAudioTs = null; return [arrayBuffer.slice(2)]; }
  // With n_red > 0 the bytes after the flag word are headers, not Opus, so a
  // truncated fixed part leaves no primary to salvage.
  if (arrayBuffer.byteLength < 6 + nRed * 4 + 1) { lastAudioTs = null; return []; }
  const pts = ((bytes[2] << 24) | (bytes[3] << 16) | (bytes[4] << 8) | bytes[5]) >>> 0;
  let pos = 6;
  const offsets = [], lens = [];
  for (let i = 0; i < nRed; i++) {
    const field = (bytes[pos + 1] << 16) | (bytes[pos + 2] << 8) | bytes[pos + 3];
    offsets.push((field >> 10) & 0x3fff);
    lens.push(field & 0x3ff);
    pos += 4;
  }
  pos += 1;
  // The declared block lengths must fit the payload: slice() clamps silently,
  // and the primary cannot be located without trustworthy lengths.
  let declared = pos;
  for (let i = 0; i < nRed; i++) { declared += lens[i]; }
  if (declared > arrayBuffer.byteLength) { lastAudioTs = null; return []; }
  const blocks = [];
  for (let i = 0; i < nRed; i++) {
    blocks.push({ ts: (pts - offsets[i]) >>> 0, buf: arrayBuffer.slice(pos, pos + lens[i]) });
    pos += lens[i];
  }
  blocks.push({ ts: pts, buf: arrayBuffer.slice(pos) });
  if (lastAudioTs === null) {
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

/**
 * Starts the WebSocket streaming core in this page. Everything below is
 * closure state of one session; the public surface is the `window` contract
 * described in the module docblock.
 */
export default function websockets() {
let isSecondaryDisplayConnected = false;
/** Display rectangles (+ per-page scale) from the last DISPLAY_CONFIG_UPDATE. */
let latestDisplayLayouts = null;
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
/**
 * Concealment counters: zero-filled underrun samples and drop-oldest events
 * reported by the playback AudioWorklet, and the main thread's drop-gate hits.
 */
window.currentAudioUnderrunSamples = 0;
window.currentAudioWorkletDropped = 0;
window.currentAudioDropped = 0;
/**
 * Track-generator sink: decoded VideoFrames are presented through a `<video>`
 * element (GPU-composited, no per-frame 2D-canvas draw) by
 * MediaStreamTrackGenerator on the main thread (Chromium) or the standard
 * worker-only VideoTrackGenerator whose track is transferred back here.
 * Full-frame H.264 modes only; striped and JPEG modes, and browsers with
 * neither generator, keep the canvas path.
 */
let videoElement = null;
let videoFrameWriter = null;
let videoTrack = null;
let mstgActive = false;
let mstgLastGeom = null;
/**
 * Consecutive frames the generator sink dropped for backpressure. A generator
 * whose consumer stopped pulling (a hide/resume starve) stays backpressured
 * forever, so the sink is rebuilt once the count reaches
 * `SINK_STALL_DROP_LIMIT`; any successful write resets it.
 */
let mstgConsecutiveDrops = 0;
const SINK_STALL_DROP_LIMIT = 30;
/**
 * Handoff gate: the main canvas is hidden only once the takeover sink has
 * provably rendered a frame (requestVideoFrameCallback for a `<video>`, a
 * one-time `presented` message for the worker's OffscreenCanvas). Hiding it on
 * the first write flashes black, since the first track frame can arrive before
 * the `<video>` renders and the worker draws asynchronously. `sinkRevealGen`
 * invalidates stale callbacks across deactivate/re-activate.
 */
let mstgRendered = false;
let videoWorkerRendered = false;
let sinkRevealGen = 0;
/**
 * Set by the canvas-style writers (applyManualCanvasStyle, resetCanvasStyle,
 * updateCanvasImageRendering); the present paths re-mirror the canvas box onto
 * the `<video>` or worker canvas only while it is set, instead of serializing
 * cssText every frame.
 */
let canvasGeomDirty = true;
let jpegStripeRenderQueue = [];
/** JPEG stripes handed to a decoder that have not reached the queue yet. */
let jpegStripeDecodesPending = 0;
let isVideoPipelineActive = true;
let isAudioPipelineActive = true;
let isMicrophoneActive = false;
let isWebcamActive = false;
let isGamepadEnabled;
let lastReceivedVideoFrameId = -1;
/** When that id arrived, so an ack can report how long it was held. */
let lastReceivedVideoFrameAt = 0;
let initializationComplete = false;
let audioEnabled = true;
let microphoneEnabled = true;
let webcamEnabled = true;
let webcamCapture = null;
/** Codec the webcam uplink is encoding with, or null while idle. The frames
 * themselves go worker to worker over a port, so this is the only place the
 * page can say what they are. */
Object.defineProperty(window, 'webcamCodec', {
  configurable: true,
  get: () => (webcamCapture ? webcamCapture.codec : null),
});
// webcam_encoder: the server default, overridden by the stored choice unless locked.
let webcamEncoderPreference = 'auto';
let preferredWebcamDeviceId = null;
let displayId = 'primary';
let displayPosition = 'right';
const PER_DISPLAY_SETTINGS = [
    'framerate', 'video_crf', 'video_fullcolor',
    'video_streaming_mode', 'jpeg_quality', 'paint_over_jpeg_quality', 'use_cpu',
    'video_paintover_crf', 'video_paintover_burst_frames', 'use_paint_over_quality',
    'manual_resolution', 'manual_width', 'manual_height',
    'encoder', 'scaleLocallyManual', 'use_browser_cursors', 'rate_control_mode',
    'video_bitrate', 'force_aligned_resolution'
];
let micStream = null;
let micAudioContext = null;
let micSourceNode = null;
let micWorkletNode = null;
let micEncodeWorker = null;
let preferredInputDeviceId = null;
let preferredOutputDeviceId = null;
let metricsIntervalId = null;
let backpressureIntervalId = null;
let reconnectIntervalId = null;
/**
 * Watchdog for a START_VIDEO lost while the tab was hidden, which would leave a
 * black stream. Armed when the tab becomes visible, cleared on the first
 * VIDEO_STARTED or video chunk.
 */
let startVideoWatchdogTimer = null;
let startVideoWatchdogAttempts = 0;
const START_VIDEO_WATCHDOG_MS = 3000;
const START_VIDEO_WATCHDOG_MAX_ATTEMPTS = 3;
/**
 * How long a tab that just became visible waits for a frame before treating
 * the stream as stopped rather than idle.
 */
const VISIBLE_FRAME_PROBE_MS = 2500;
/**
 * Shared-mode stall watchdog. A shared viewer's stream can die mid-session
 * without notification (the controller's tab-hide stops the broadcast
 * encoder) after the one-shot START_VIDEO watchdog is already cleared. While
 * visible, ready and unpaused, a gap in video chunks resends START_VIDEO (the
 * server both resyncs a live capture and restarts a dead one), with
 * exponential backoff so a static stream is not spammed.
 */
let sharedStallWatchdogId = null;
let sharedDecoderUnavailableNoticed = false;
let lastSharedVideoChunkTime = 0;
let sharedStallRecoveryAttempts = 0;
let sharedStallNextRecoveryTime = 0;
const SHARED_STALL_TIMEOUT_MS = 3000;
const SHARED_STALL_MAX_BACKOFF_MS = 30000;
const METRICS_INTERVAL_MS = 500;
const BACKPRESSURE_INTERVAL_MS = 50;
/** How often the socket worker re-reports a draining send buffer. */
const BUFFERED_DRAIN_MS = 20;
/**
 * Raw bytes per clipboard chunk, before base64 expansion, and the socket
 * backlog a transfer waits below before queueing the next.
 *
 * Sized for latency, not capacity: the microphone and every input event share
 * this one ordered socket, and together these bound what they can find queued
 * ahead of them. Well under any server receive ceiling, so the advertised one
 * is not consulted. The chunk is a multiple of 3, to concatenate as base64.
 */
const CLIPBOARD_CHUNK_SIZE = 16383;
const CLIPBOARD_BACKLOG_BYTES = 64 * 1024;
/** Backlog re-check interval; the socket worker re-reports as the socket drains. */
const CLIPBOARD_BACKLOG_POLL_MS = 20;
window.manual_resolution = false;
let manual_width = null;
let manual_height = null;
let originalWindowResizeHandler = null;
let handleResizeUI_globalRef = null;
let vncStripeDecoders = {};
/**
 * Chunks one stripe decoder may have outstanding before its deltas are
 * dropped. Past it the row is gated until its next IDR: decoding a backlog
 * late only deepens it, and a dropped delta breaks the row's reference chain.
 * The video worker holds its full-frame decoder to the same contract.
 */
const STRIPE_DECODE_QUEUE_LIMIT = 8;
/**
 * A stripe is stale only when it trails the last drawn id by at most this
 * many frames. The id is a uint16, so a larger modular gap means the row sat
 * static for a long time or the id wrapped; drawing such a stripe avoids
 * wedging the row for up to half the id space.
 */
const JPEG_STRIPE_REORDER_WINDOW = 256;
let stripeDecodeSoftErrors = {};
let wakeLockSentinel = null;
let currentEncoderMode = 'h264enc-striped';
let useCssScaling = false;
let trackpadMode = false;
let scalingDPI = 96;
/** `scaling_dpi` stops in 25% steps; densities between them snap to the nearest. */
const DPI_STOPS = [96, 120, 144, 168, 192, 216, 240, 264, 288];
/**
 * Derives the default `scaling_dpi` from the local display density so the
 * remote desktop's UI matches the local one.
 * @returns {number} The nearest entry of `DPI_STOPS`, clamped at both ends.
 */
function autoDeriveDpi() {
  const dpr = window.devicePixelRatio || 1;
  const target = Math.round(dpr * 4) * 24;
  return DPI_STOPS.reduce((prev, cur) =>
    Math.abs(cur - target) < Math.abs(prev - target) ? cur : prev);
}

let lastFollowedDpr = window.devicePixelRatio || 1;
/**
 * Follows a live devicePixelRatio change while `scaling_dpi` sits on its
 * automatic default, re-deriving and pushing it so the remote UI density
 * matches the display the window is on. Called from both the resize handler
 * and the matchMedia density watcher: an OS scaling change can surface as
 * either, and emulated density changes fire only the resize.
 * @returns {void}
 */
function maybeFollowDpr() {
  const dpr = window.devicePixelRatio || 1;
  if (dpr === lastFollowedDpr) return;
  lastFollowedDpr = dpr;
  if (isSharedMode) return;
  if (getStringParam('scaling_dpi', null) !== null) return;
  const derived = autoDeriveDpi();
  if (derived === scalingDPI) return;
  scalingDPI = derived;
  console.log(`DPI follows devicePixelRatio: scaling_dpi -> ${derived}.`);
  sendFullSettingsUpdateToServer('devicePixelRatio changed');
}
let antiAliasingEnabled = true;
let clipboard_in_enabled = true;
let clipboard_out_enabled = true;
/**
 * The cursor-rendering preference in force: seeded from localStorage at init,
 * then updated by a dashboard pick (persisted) or a server-pushed value (not
 * persisted, so a later server-side change stays re-pushable).
 */
let use_browser_cursors = true;
/**
 * Applies the cursor preference to the input handler, forced to browser
 * cursors whenever a second display is involved, and posts the value in
 * effect as `effectiveCursorState` so the dashboard toggle reflects the
 * override rather than the preference alone.
 */
function applyEffectiveCursorSetting() {
    const userPreference = use_browser_cursors;
    const isMultiMonitorActive = (displayId === 'display2' || (displayId === 'primary' && isSecondaryDisplayConnected));
    const finalSetting = isMultiMonitorActive ? true : userPreference;
    if (window.webrtcInput && typeof window.webrtcInput.setUseBrowserCursors === 'function') {
        console.log(`Applying effective cursor setting. Multi-monitor: ${isMultiMonitorActive}, User Pref: ${userPreference}, Final: ${finalSetting}`);
        window.webrtcInput.setUseBrowserCursors(finalSetting);
    }
    try {
        window.postMessage({ type: 'effectiveCursorState', value: finalSetting }, window.location.origin);
    } catch (e) { /* postMessage unavailable */ }
}
/** Publishes the real viewport height as the `--vh` CSS unit (mobile browser chrome excluded). */
function setRealViewportHeight() {
  const vh = window.innerHeight * 0.01;
  document.documentElement.style.setProperty('--vh', `${vh}px`);
}
/** One id per multipart clipboard transfer. */
let clipboardTransferCounter = 0;
const clipboardWorker = new ClipboardWorkerBridge();
/**
 * PNG-normalizes an image on the worker, which is where the decode and
 * re-encode of a large one belong; the page's own canvas covers a worker that
 * cannot do it.
 */
const reencodePngOffThread = (blob) => clipboardWorker.reencodePng(blob).then((r) => r.result);
let enable_binary_clipboard = true;
/**
 * Server-clipboard cache, change-only sync and Ctrl/Cmd+C request queue
 * (lib/clipboard-sync.js); the send hook late-binds `websocket`.
 */
const clipboardSync = createClipboardSync({
    sendRequest: () => {
        if (websocket && websocket.readyState === WebSocket.OPEN) {
            websocket.send('REQUEST_CLIPBOARD');
        }
    },
    digestBytes: async (buf) => {
        const { byteLength, hash } = await clipboardWorker.hashBytes(buf);
        return digestedPayload(byteLength, hash);
    }
});
/**
 * Retry queue for clipboard writes pushed by the server: they carry no user
 * activation, and Firefox and WebKit reject the write until the next gesture.
 */
const deferredClipboardWriter = createDeferredClipboardWriter();
/** Multipart download state and connect-time cache-only fetch (`cr`) tracking, shared with the WebRTC core. */
const multipartClipboard = createMultipartClipboardState(
  (mime) => clipboardWorker.decodeStream(mime));
const taggedClipboardFetch = createTaggedClipboardFetch();
const armTaggedClipboardReply = () => taggedClipboardFetch.arm();
const consumeInitClipboardFetch = () => taggedClipboardFetch.consume();
/** Local-to-server clipboard sync, built with the connection; the dashboard's own pushes go through it. */
let localClipboardSender = null;

/**
 * Sends content the user named (the clipboard box, the image upload) so it
 * outranks the focus read. Refused before the connection builds the sender.
 * @param {string|ArrayBuffer|Blob} data
 * @param {string} [mime]
 * @param {Function} [onSkip] Told why nothing was sent.
 * @returns {Promise<void>}
 */
function sendExplicitClipboard(data, mime, onSkip) {
  if (!localClipboardSender) {
    if (onSkip) onSkip('not connected', 'clipboardSkipNotConnected');
    return Promise.resolve();
  }
  return localClipboardSender.sendExplicit(data, mime, onSkip);
}

let detectedSharedModeType = null;
let playerInputTargetIndex = 0;

const urlParams = new URLSearchParams(window.location.search);
const authToken = urlParams.get('token');

/**
 * The page hash selects the role: `#display2[-position]` is the secondary
 * display in every auth mode (a token-authenticated page that connected as
 * `primary` would supersede the page already holding it), and without a token
 * `#shared` and `#player2` to `#player4` are the shared-viewer roles.
 */
const hash = window.location.hash;
if (hash.startsWith('#display2')) {
    displayId = 'display2';
    const parts = hash.split('-');
    if (parts.length > 1) {
        const position = parts[1];
        if (['left', 'right', 'up', 'down'].includes(position)) {
            displayPosition = position;
        }
    }
}

if (authToken) {
    isTokenAuthMode = true;
    console.log("Client is running in Token Authentication mode.");
} else {
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
}
/** Shared-viewer handshake state: `idle`, `ready` or `error`. */
let sharedClientState = 'idle';
/**
 * Whether this shared viewer paused its own video feed on tab-hide; the server
 * drops just this socket from the broadcast while control, cursor and audio stay.
 */
let sharedVideoPaused = false;
let isSharedMode = detectedSharedModeType !== null;
/**
 * Whether the server executes `cmd,` messages, mirroring its `command_enabled`
 * setting; true until a server_settings payload says otherwise, so a server
 * that never advertises the key leaves commands enabled.
 */
let serverCommandEnabled = true;

if (isSharedMode) {
  console.log(`Client is running in ${detectedSharedModeType} mode.`);
}
if (displayId === 'display2') {
    console.log("Client is running in Secondary Display mode.");
}
window.onload = () => {
  'use strict';
};

const storageAppName = getStorageAppName();
/**
 * localStorage write that degrades a full or unavailable store to a warning
 * instead of throwing QuotaExceededError into the caller.
 * @param {string} key
 * @param {string} value
 */
const safeSetItem = (key, value) => {
  try {
    window.localStorage.setItem(key, value);
  } catch (e) {
    console.warn(`Selkies: could not persist '${key}' to localStorage:`, e);
  }
};

/**
 * Storage key of the software-decode preference. A hardware H.264 decoder can
 * accept a config and then fail at decode(), which isConfigSupported cannot
 * predict, so the first hard decoder error retries the same encoder on
 * software before the fallback ladder reloads the page and degrades the
 * stream. The choice is stored against the user agent: a client whose
 * hardware path is broken starts on software, while a browser update (usually
 * a new decoder stack) re-probes hardware.
 */
const SOFTWARE_DECODE_KEY = `${storageAppName}_prefer_software_decode`;
let preferSoftwareDecode = false;
try {
  preferSoftwareDecode =
    window.localStorage.getItem(SOFTWARE_DECODE_KEY) === navigator.userAgent;
} catch (e) {
  console.warn('Selkies: could not read the software-decode preference:', e);
}
/**
 * Whether the one software-decode retry of this session is spent. It is spent
 * whether or not the engine honoured the hint, so an engine that ignores it
 * cannot loop the retry. Errors within `SOFTWARE_DECODE_SETTLE_MS` of the
 * switch describe the path just torn down (the striped modes run a decoder
 * per stripe and they fail together) and are absorbed instead of reaching
 * the ladder.
 */
let softwareDecodeAttempted = preferSoftwareDecode;
let softwareDecodeSwitchedAt = Number.NEGATIVE_INFINITY;
const SOFTWARE_DECODE_SETTLE_MS = 3000;
/**
 * Persists or clears the software-decode preference.
 * @param {boolean} enabled
 */
const rememberSoftwareDecode = (enabled) => {
  if (videoWorker) {
    try { videoWorker.postMessage({ type: 'wireHints', software: enabled }); } catch (e) { /* respawns fresh */ }
  }
  preferSoftwareDecode = enabled;
  if (enabled) {
    safeSetItem(SOFTWARE_DECODE_KEY, navigator.userAgent);
    return;
  }
  try {
    window.localStorage.removeItem(SOFTWARE_DECODE_KEY);
  } catch (e) {
    console.warn('Selkies: could not clear the software-decode preference:', e);
  }
};
/**
 * Applies the acceleration preference to a VideoDecoder config; every decoder
 * (main, stripe, SPS-driven, worker) goes through here so they agree. Unset,
 * the UA default picks a hardware decoder when one works.
 * @param {VideoDecoderConfig} config
 * @returns {VideoDecoderConfig}
 */
const decoderConfigFor = (config) =>
  preferSoftwareDecode ? { ...config, hardwareAcceleration: 'prefer-software' } : config;

/**
 * Storage key of the decoder crash count the fallback ladder escalates on.
 * The count describes the current troubled stretch, not a lifetime total: a
 * session that decodes video for `HEALTHY_SESSION_MS` retires it, otherwise
 * unrelated faults months apart accumulate until the ladder pins jpeg.
 */
const CRASH_COUNT_KEY = `${storageAppName}_crash_count`;
const HEALTHY_SESSION_MS = 60000;
let crashCountRetired = false;
/** Clears the crash count once this session has proven healthy; runs on every metrics tick. */
const retireCrashCountWhenHealthy = () => {
  if (crashCountRetired || isSharedMode) return;
  if (!(window.fps > 0) || performance.now() < HEALTHY_SESSION_MS) return;
  crashCountRetired = true;
  try {
    window.localStorage.removeItem(CRASH_COUNT_KEY);
  } catch (e) {
    console.warn('Selkies: could not clear the decoder crash count:', e);
  }
};

document.title = 'Selkies';
fetch('manifest.json')
  .then(response => response.json())
  .then(manifest => {
    if (manifest.name) {
      document.title = manifest.name;
    }
  })
  .catch(() => {
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
let videoBitrate = 8000;
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
/**
 * Nudges the encoder with a few keyframe requests after the handshake until
 * the first frame lands; a freshly connected page has no keyframe loop of its own.
 */
let firstFrameRecoveryTimer = null;
let inputInitialized = false;
let scaleLocallyManual;
window.fps = 0;
window.videoChunksReceived = 0;
window.videoDivertOn = false;
/** Stripe row layout the video worker is decoding while the divert holds. */
window.videoStripeRows = {};
let frameCount = 0;
let uniqueStripedFrameIdsThisPeriod = new Set();
let lastStripedFpsUpdateTime = performance.now();
let lastFpsUpdateTime = performance.now();
let statusDisplayElement;
let playButtonElement;
let overlayInput;
let rateControlMode = 'crf';

/**
 * Reads an integer setting from localStorage under the app prefix; keys in
 * `PER_DISPLAY_SETTINGS` carry a `_display2` suffix on the secondary display.
 * The get/set helpers below share that key scheme.
 * @param {string} key
 * @param {number|null} default_value Returned when the key is unset.
 * @returns {number|null}
 */
const getIntParam = (key, default_value) => {
  const prefixedKey = `${storageAppName}_${key}`;
  let finalKey = prefixedKey;
  if (displayId === 'display2' && PER_DISPLAY_SETTINGS.includes(key)) {
    finalKey = `${prefixedKey}_${displayId}`;
  }
  const value = window.localStorage.getItem(finalKey);
  return (value === null || value === undefined) ? default_value : parseInt(value);
};
/** Float variant of getIntParam, for range settings with fractional bounds. */
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
/** Stores an integer setting; `null` removes the key. */
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
/** The localStorage key a setting is stored under, display suffix included. */
const prefixedStorageKey = (key) => {
  const prefixedKey = `${storageAppName}_${key}`;
  return (displayId === 'display2' && PER_DISPLAY_SETTINGS.includes(key))
    ? `${prefixedKey}_${displayId}` : prefixedKey;
};
/** Reads a boolean setting stored as `'true'`/`'false'`. */
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
/** Stores a boolean setting; `null` removes the key. */
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
/** Reads a string setting. */
const getStringParam = (key, default_value) => {
  const prefixedKey = `${storageAppName}_${key}`;
  let finalKey = prefixedKey;
  if (displayId === 'display2' && PER_DISPLAY_SETTINGS.includes(key)) {
    finalKey = `${prefixedKey}_${displayId}`;
  }
  const value = window.localStorage.getItem(finalKey);
  return (value === null || value === undefined) ? default_value : value;
};
/** Stores a string setting; `null` removes the key. */
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
/**
 * Reconciles the stored settings with the server's `server_settings` payload
 * and mirrors the result onto `window[key]` for the runtime.
 *
 * Only genuine user overrides are persisted: a server value with no stored
 * override is applied to the runtime but never written to localStorage, so a
 * later server-side change can still be re-pushed. A stored value outside the
 * server's range or allowed list is dropped back to the server default, and a
 * locked setting always wins at runtime without touching the user's key.
 * The stored key can differ from the server's name (storageKeyForServerKey:
 * HiDPI stores as `useCssScaling`), and range settings are read as floats so
 * a fractional pick survives. An operator-overridden boolean with no stored
 * pick is reported as a change so the runtime consumers apply it; a plain
 * value (`audio_channels`) configures pipelines rather than preferences and
 * is mirrored only.
 * @param {Object<string, Object>} serverSettings Per-key descriptors carrying
 *     `value`, `default`, `min`, `max`, `allowed`, `locked` and `overridden`.
 * @returns {Object<string, *>} Settings whose effective value changed and must
 *     be applied by the caller.
 */
/**
 * The `use_css_scaling` the deployment implies, off the shared ladder: a locked
 * or operator value, else the client's explicit pick, else CSS scaling whenever
 * a resolution is configured. Resolved here rather than left to a settings
 * panel, which a dashboard may mount only when it is opened -- until then the
 * session would stream pixel-perfect against a configuration that says
 * otherwise, on every load.
 * @param {Object<string, Object>} serverSettings The `server_settings` payload.
 * @returns {boolean}
 */
function resolvedCssScaling(serverSettings) {
  const key = prefixedStorageKey('useCssScaling');
  const explicit = window.localStorage.getItem(`${key}_explicit_choice`) === 'true';
  const hidpi = resolveSpec(HIDPI_SPEC, serverSettings,
    { manualActive: !!window.manual_resolution || manual_width > 0 },
    () => (explicit ? window.localStorage.getItem(key) : null));
  return HIDPI_SPEC.toServer(hidpi);
}

function sanitizeAndStoreSettings(serverSettings) {
  console.log("Sanitizing and storing settings based on server payload.");
  const changes = {};

  const storageKeyFor = (key) => {
    const prefixedKey = `${storageAppName}_${key}`;
    return (displayId === 'display2' && PER_DISPLAY_SETTINGS.includes(key))
      ? `${prefixedKey}_${displayId}` : prefixedKey;
  };

  for (const key in serverSettings) {
    if (!serverSettings.hasOwnProperty(key)) continue;
    const setting = serverSettings[key];
    const storeKey = storageKeyForServerKey(key);
    const finalKey = storageKeyFor(storeKey);
    const wasUnset = window.localStorage.getItem(finalKey) === null;

    if (setting.min !== undefined && setting.max !== undefined) {
      const clientValue = getFloatParam(storeKey, setting.default);
      if (wasUnset) {
        window[key] = clientValue;
      } else if (clientValue < setting.min || clientValue > setting.max) {
        console.log(`Sanitizing '${key}': stored value ${clientValue} out of range [${setting.min}-${setting.max}]. Reverting to server default ${setting.default}.`);
        window.localStorage.removeItem(finalKey);
        window[key] = setting.default;
        changes[key] = setting.default;
      } else {
        window[key] = clientValue;
      }
    }
    else if (setting.allowed !== undefined) {
      const isNumericEnum = !isNaN(parseFloat(setting.allowed[0]));
      const clientValueStr = isNumericEnum
        ? getIntParam(storeKey, parseInt(setting.value, 10)).toString()
        : getStringParam(storeKey, setting.value);
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
        if (isNumericEnum) setIntParam(storeKey, parseInt(clientValueStr, 10));
        else setStringParam(storeKey, clientValueStr);
      }
    }
    else if (typeof setting.value === 'boolean') {
      const serverValue = setting.value;
      if (setting.locked) {
        const clientValue = getBoolParam(storeKey, !serverValue);
        if (clientValue !== serverValue) {
          console.log(`Sanitizing '${key}': setting is locked by server. Client value ${clientValue} is being overwritten with ${serverValue}.`);
          changes[key] = serverValue;
        }
        window[key] = serverValue;
      } else if (wasUnset) {
        window[key] = serverValue;
        if (setting.overridden) {
          changes[key] = serverValue;
        }
      } else {
        const clientValue = getBoolParam(storeKey, serverValue);
        window[key] = clientValue;
        setBoolParam(storeKey, clientValue);
      }
    }
    else if (setting.value !== undefined) {
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
webcamEncoderPreference = getStringParam('webcam_encoder', 'auto');
scaleLocallyManual = getBoolParam('scaleLocallyManual', true);
window.manual_resolution = getBoolParam('manual_resolution', false);
isGamepadEnabled = getBoolParam('isGamepadEnabled', true);
useCssScaling = getBoolParam('useCssScaling', false);
trackpadMode = getBoolParam('trackpadMode', false);
rateControlMode = getStringParam('rate_control_mode', rateControlMode);
videoBitrate = getIntParam('video_bitrate', videoBitrate);
if (getStringParam('scaling_dpi', null) === null) {
  scalingDPI = autoDeriveDpi();
} else {
  scalingDPI = getIntParam('scaling_dpi', 96);
}
antiAliasingEnabled = getBoolParam('antiAliasingEnabled', true);
use_browser_cursors = getBoolParam('use_browser_cursors', true);
enable_binary_clipboard = getBoolParam('enable_binary_clipboard', enable_binary_clipboard);
clipboard_in_enabled = getBoolParam('clipboard_in_enabled', true);
clipboard_out_enabled = getBoolParam('clipboard_out_enabled', true);
force_aligned_resolution = getBoolParam('force_aligned_resolution', force_aligned_resolution);

if (isSharedMode) {
    manual_width = 1280;
    manual_height = 720;
    console.log(`Shared mode: Initialized manual_width/Height to ${manual_width}x${manual_height}`);
} else {
    manual_width = getIntParam('manual_width', null);
    manual_height = getIntParam('manual_height', null);
}

/**
 * Gaming mode is fullscreen holding the pointer and the keyboard; plain
 * fullscreen holds neither. A transport switch rebuilds Input, so the mode it
 * was in is carried over.
 */
let gamingModeActive = false;

/**
 * Enters fullscreen through the input handler, which owns both modes; before
 * it exists only plain fullscreen is possible.
 * @param {boolean} gaming Whether to hold the pointer and the keyboard.
 */
const enterFullscreen = (gaming) => {
  gamingModeActive = !!gaming;
  const input = window.webrtcInput;
  if (input && gaming && typeof input.enterGamingMode === 'function') {
    input.enterGamingMode();
  } else if (input && typeof input.enterFullscreen === 'function') {
    input.enterFullscreen();
  } else if (document.fullscreenElement === null) {
    document.documentElement.requestFullscreen().catch(() => {});
  }
};

/** Hides the start overlay and keeps the screen awake once the user starts the stream. */
const playStream = () => {
  showStart = false;
  if (playButtonElement) playButtonElement.classList.add('hidden');
  if (statusDisplayElement) statusDisplayElement.classList.add('hidden');
  requestWakeLock();
  console.log("playStream called in WebSocket mode - UI elements hidden.");
};

/**
 * Shows `loadingText`, or else the sentence-cased `status` word (the internal
 * value stays lower-case for comparisons).
 */
const updateStatusDisplay = () => {
  if (statusDisplayElement) {
    const _statusText = loadingText || status;
    statusDisplayElement.textContent = _statusText ? _statusText.charAt(0).toUpperCase() + _statusText.slice(1) : _statusText;
  }
};

/**
 * Prefixes a log line with the wall-clock time.
 * @param {string} msg
 * @returns {string}
 */
window.applyTimestamp = (msg) => {
  const now = new Date();
  const ts = `${now.getHours()}:${now.getMinutes()}:${now.getSeconds()}`;
  return `[${ts}] ${msg}`;
};

/**
 * Floors a dimension to the encoder's alignment: 16 when
 * `force_aligned_resolution` is set, 2 otherwise.
 * @param {number} num
 * @returns {number}
 */
const alignResolution = (num) => {
  const alignment = force_aligned_resolution ? 16 : 2;
  return Math.floor(num / alignment) * alignment;
};

/**
 * Whether this is a Chromium engine (not the WebKit-backed iOS Chrome): the
 * userAgentData brands are the authoritative signal, `window.chrome` the
 * fallback for older engines.
 */
const isChromium = (() => {
  const isIOS = /iPad|iPhone|iPod/.test(navigator.userAgent) ||
                (navigator.platform === 'MacIntel' && navigator.maxTouchPoints > 1);
  const isFirefox = /Firefox|FxiOS/.test(navigator.userAgent);
  const isCriOS = /CriOS/.test(navigator.userAgent);
  const brands = (navigator.userAgentData && navigator.userAgentData.brands) || [];
  const isChromiumBrand = brands.some((b) => /Chromium|Google Chrome/.test(b.brand));
  const hasChromeObj = typeof window.chrome !== 'undefined';
  return (isChromiumBrand || hasChromeObj) && !isIOS && !isFirefox && !isCriOS;
})();

/**
 * Whether the page has MediaStreamTrackGenerator (Chromium only). The standard
 * VideoTrackGenerator is a DedicatedWorker global, so only the video worker can
 * report it, which is why the worker is started before this is consulted.
 *
 * Sink priority: the worker's VideoTrackGenerator, then this, then the worker's
 * OffscreenCanvas, then the page's own canvas. Which of the generators an engine
 * has is probed, never assumed: both come and go with browser releases.
 */
const supportsWindowMSTG = (typeof MediaStreamTrackGenerator !== 'undefined');

/**
 * Whether the worker video sink is enabled (`?offscreen_worker=false` turns
 * it off). The worker hosts either the standard VideoTrackGenerator, whose
 * MediaStreamTrack is transferred back for `<video>.srcObject`, or, where it
 * has none, an OffscreenCanvas it composites onto.
 */
let USE_OFFSCREEN_WORKER = false;
let videoWorker = null;
/**
 * Canvas the worker composites on in `canvas` mode; separate from the main
 * canvas so the JPEG-stripe path is unaffected.
 */
let videoWorkerCanvas = null;
/** Composite striped-codec stripes in the worker; capability-gated at first use. */
let stripeCompositeEnabled = true;
let videoWorkerActive = false;
let videoWorkerReady = false;
/** Sink the worker reported from its self-probe: `vtg`, `canvas`, or `null` while handshaking. */
let videoWorkerMode = null;
/** VideoTrackGenerator track transferred back from the worker in `vtg` mode. */
let videoWorkerTrack = null;
let videoWorkerCanvasTransferred = false;
let videoWorkerLastGeom = null;
/**
 * Frames in flight to the worker, which acks each one it consumed; new frames
 * are dropped at the cap so GPU VideoFrames cannot pile up and stall the decoder.
 */
let videoWorkerInFlight = 0;
const VIDEO_WORKER_MAX_IN_FLIGHT = 3;
/**
 * Whether the worker hosts the VideoDecoder (non-shared full-frame H.264 on
 * Safari and Firefox), so decode and present both stay off the main thread
 * and only encoded bytes cross the boundary. The `workerDecoder*` fields track
 * the config last pushed to it; `workerKeyframeCodec` is the SPS codec of the
 * last keyframe, sticky on a parse miss, since a codec string that oscillates
 * reconfigures the worker decoder and requests a keyframe at every boundary,
 * stalling playback; `workerDecodeFailed` sticks on a worker decoder error
 * and routes decoding back to the main thread, with the worker sink or the 2D
 * canvas presenting.
 */
let decodeInWorker = false;
let workerDecoderCodec = null, workerDecoderW = 0, workerDecoderH = 0;
let workerKeyframeCodec = null;
let workerDecodeFailed = false;
// Whether 0x03/0x04 currently bypass the page (see updateVideoDivert), and
// the wire frames the video worker counted since the last metrics tick.
let videoDivertOn = false;
/** Whether the running divert is the striped one, which acks presented ids. */
let videoDivertStriped = false;
let divertedWireFramesThisPeriod = 0;
/** What the worker reported it can decode, for the striped divert gates. */
let videoWorkerStripedDecode = false, videoWorkerJpegDecode = false;
/** Worker spawns attempted, capping respawn churn where Worker() cannot run. */
let videoWorkerSpawnAttempts = 0;
/**
 * A transferred canvas whose element never mirrors the worker's committed
 * size is a placeholder nothing will ever display (Playwright's WebKit);
 * latched after a grace period so presentation returns to the page canvas.
 */
let videoWorkerSinkInert = false;
let workerSinkInertSince = 0;

/**
 * Watches the canvas-mode placeholder actually take committed frames: its
 * element mirrors the worker-side size on the first commit everywhere the
 * sink works, so one that stays at the default size while the worker reports
 * presenting is inert, and presentation falls back to the page canvas.
 * @returns {void}
 */
function checkWorkerSinkAlive() {
  const inertNow = videoWorkerMode === 'canvas' && videoWorkerRendered &&
    videoWorkerCanvas && canvas && canvas.width >= 640 && videoWorkerCanvas.width < 640;
  if (!inertNow) { workerSinkInertSince = 0; return; }
  const now = performance.now();
  if (!workerSinkInertSince) { workerSinkInertSince = now; return; }
  if (now - workerSinkInertSince > 1500) {
    videoWorkerSinkInert = true;
    console.info('[Selkies] worker canvas placeholder never took a committed frame; presenting on the page canvas instead.');
    deactivateVideoWorker();
  }
}
/**
 * Reads the codec string from a keyframe's SPS: scans the Annex-B payload for
 * the first SPS NAL and builds `avc1.PPCCLL` from it.
 * @param {Uint8Array} bytes
 * @returns {string|null} `null` when no SPS is found, so the caller falls back
 *     to the heuristic guess.
 */
const parseAvcCodecFromAnnexB = (bytes) => {
  if (!bytes || bytes.length < 5) return null;
  const hex2 = (n) => n.toString(16).toUpperCase().padStart(2, '0');
  const n = bytes.length;
  let i = 0;
  while (i + 3 < n) {
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
      // profile_idc, constraint flags and level_idc are the first three RBSP
      // bytes and, with profile_idc always >= 66, never need emulation prevention.
      if (nalStart + 3 < n) {
        const profileIdc = bytes[nalStart + 1];
        const constraintFlags = bytes[nalStart + 2];
        const levelIdc = bytes[nalStart + 3];
        return `avc1.${hex2(profileIdc)}${hex2(constraintFlags)}${hex2(levelIdc)}`;
      }
      return null;
    }
    i = nalStart;
  }
  return null;
};

const VIDEO_WORKER_SRC = `
// Video sink and optional in-worker decoder. The sink is a worker-only
// VideoTrackGenerator (its track transferred to the page for <video>.srcObject) or a
// transferred OffscreenCanvas. Encoded chunks are decoded here so no decoded frame
// crosses the thread boundary; a frame transferred in (m.frame) is the warm-up path.
let mode = null, oc = null, ctx = null, writer = null, closed = false, presented = false;
let dec = null, decKey = false, decNeedKey = false;
// Consecutive backpressure drops; a stalled consumer never resumes on its own.
let sinkDrops = 0;
// Decode backlog (frames) above which deltas are dropped.
const OVERLOAD_QUEUE = 24;
// Keyframe-request throttle while decode is backed up.
let lastNeedKey = 0;
const sendNeedKey = (reason) => {
  const now = Date.now();
  if (now - lastNeedKey < 800) return;
  lastNeedKey = now;
  self.postMessage({ type: 'needKeyframe', reason });
};
const ack = () => self.postMessage({ ack: true });

// Present one decoded VideoFrame on the active sink. Consumes/closes the frame.
function present(f) {
  if (mode === 'vtg' && writer && !closed) {
    // Drop on sink backpressure.
    if (writer.desiredSize !== null && writer.desiredSize <= 0) {
      f.close();
      if (++sinkDrops >= 30) { closed = true; self.postMessage({ type: 'error' }); }
      return;
    }
    sinkDrops = 0;
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
  wireCodec = null; wireW = 0; wireH = 0;
}

function configureDecoder(codec, w, h, software) {
  closeDecoder();
  try {
    dec = new VideoDecoder({ output: present, error: () => { closeDecoder(); self.postMessage({ type: 'decoderError' }); } });
    // configure() is synchronous, so the next chunk decodes without an async gap and
    // an unsupported config surfaces via error(). The page owns the acceleration
    // preference; unset, the UA default takes a hardware decoder where there is one,
    // and the pinned SPS level keeps it from re-initializing mid-stream.
    const cfg = { codec: codec, codedWidth: w, codedHeight: h, optimizeForLatency: true };
    if (software) cfg.hardwareAcceleration = 'prefer-software';
    dec.configure(cfg);
    // A keyframe is required after (re)configure.
    decNeedKey = true;
    return true;
  } catch (err) { closeDecoder(); self.postMessage({ type: 'decoderError' }); return false; }
}

function decodeChunk(key, data, timestamp) {
  if (!dec || dec.state !== 'configured') return;
  if (key) { decKey = true; decNeedKey = false; }
  else {
    // No usable keyframe yet.
    if (!decKey || decNeedKey) { sendNeedKey('no_key'); return; }
    // Decode is falling behind: drop the delta (a fresh IDR cannot unclog the
    // queue) and request a throttled resync keyframe.
    if (dec.decodeQueueSize > OVERLOAD_QUEUE) { decNeedKey = true; sendNeedKey('overload'); return; }
  }
  try { dec.decode(new EncodedVideoChunk({ type: key ? 'key' : 'delta', timestamp: timestamp, data: data })); }
  catch (err) { closeDecoder(); self.postMessage({ type: 'decoderError' }); }
}

// Raw stripes routed straight from the socket worker, so decode and
// presentation never touch the page. The header is parsed here and the codec
// is derived from each keyframe's SPS; hints carry the page's fallback guess
// and acceleration preference. Wire stats go up once a second for the page's
// counters, watchdogs and fps, with the row layout this side is decoding.
let wireCodec = null, wireW = 0, wireH = 0, wireHint = null, wireSoftware = false;
let wireChunks = 0, wireFrames = 0, wireLastId = -1, wireStatsTimer = null;
const parseAvcCodecFromAnnexB = ${parseAvcCodecFromAnnexB.toString()};

// The striped modes (h264enc-striped, jpeg) decode, composite and present in
// here: a VideoDecoder per row offset or an in-worker JPEG decode, drawn onto
// a persistent back-buffer so undamaged rows survive, presented by the page's
// rule -- last row landed, or the socket and decoders proven quiet (the
// stripe clock), else at the frame-id boundary -- with the presented id going
// back over the wire port so the server paces against what reached the screen.
const createStripeClock = ${createStripeClock.toString()};
const STRIPE_DECODE_QUEUE_LIMIT = ${STRIPE_DECODE_QUEUE_LIMIT};
const JPEG_STRIPE_REORDER_WINDOW = ${JPEG_STRIPE_REORDER_WINDOW};
let stripedOn = false, wirePort = null;
let stripeDecs = {}, jpegLastRowId = {}, jpegRowHeights = {}, jpegPending = 0;
let stripeBack = null, stripeBackCtx = null;
let stripeGeomW = 0, stripeGeomH = 0, stripeGeomKnown = false;
let stripePendingId = null, stripeDirty = false, stripeBottom = false;
let stripeSoftErrors = 0, stripeSoftWindowStart = 0, settleTimer = null;
let presentedFrames = 0, wireDimsW = 0, wireDimsH = 0;
const stripeClock = createStripeClock();

function closeStripeState() {
  for (const y in stripeDecs) {
    const info = stripeDecs[y];
    try { if (info.dec.state !== 'closed') info.dec.close(); } catch (err) {}
  }
  stripeDecs = {}; jpegLastRowId = {}; jpegRowHeights = {}; jpegPending = 0;
  stripePendingId = null; stripeDirty = false; stripeBottom = false;
  if (settleTimer) { clearTimeout(settleTimer); settleTimer = null; }
}

// Grows to cover every stripe seen; an explicit geometry change rebuilds it.
function stripeEnsureBack(minW, minH) {
  const w = Math.max(stripeGeomW, minW), h = Math.max(stripeGeomH, minH);
  if (!(w > 0) || !(h > 0)) return false;
  if (!stripeBack) {
    stripeBack = new OffscreenCanvas(w, h);
    stripeBackCtx = stripeBack.getContext('2d', { desynchronized: true, alpha: false });
  } else if (stripeBack.width < w || stripeBack.height < h) {
    const old = stripeBack;
    stripeBack = new OffscreenCanvas(Math.max(old.width, w), Math.max(old.height, h));
    stripeBackCtx = stripeBack.getContext('2d', { desynchronized: true, alpha: false });
    try { stripeBackCtx.drawImage(old, 0, 0); } catch (err) {}
  }
  return true;
}

function stripeDrained() {
  if (jpegPending > 0) return false;
  for (const y in stripeDecs) { if (stripeDecs[y].meta.length > 0) return false; }
  return true;
}

function stripePresent() {
  if (!stripeBack || !stripeDirty) return;
  stripeDirty = false; stripeBottom = false;
  presentedFrames++;
  if (stripePendingId !== null && wirePort) {
    try { wirePort.postMessage({ presentedId: stripePendingId }); } catch (err) {}
  }
  // A shared viewer learns striped geometry from the composite itself.
  if (!stripeGeomKnown && (stripeBack.width !== wireDimsW || stripeBack.height !== wireDimsH)) {
    wireDimsW = stripeBack.width; wireDimsH = stripeBack.height;
    self.postMessage({ type: 'wireDims', w: wireDimsW, h: wireDimsH });
  }
  if (mode === 'vtg' && writer && !closed) {
    let f = null;
    try { f = new VideoFrame(stripeBack, { timestamp: performance.now() * 1000 }); }
    catch (err) { return; }
    present(f);
    return;
  }
  if (ctx) {
    if (oc.width !== stripeBack.width || oc.height !== stripeBack.height) {
      oc.width = stripeBack.width; oc.height = stripeBack.height;
    }
    try { ctx.drawImage(stripeBack, 0, 0); } catch (err) {}
    if (!presented) { presented = true; self.postMessage({ type: 'presented' }); }
  }
}

function stripeScheduleSettle() {
  if (settleTimer) return;
  const wait = Math.max(1, Math.ceil(stripeClock.settleWaitMs()));
  settleTimer = setTimeout(() => {
    settleTimer = null;
    if (!stripeDirty) return;
    if (stripeDrained() && stripeClock.settled()) { stripePresent(); return; }
    stripeScheduleSettle();
  }, wait);
}

// A new frame id proves the previous frame complete, so it goes up first
// unless quiet already presented it.
function stripeBoundary(frameId) {
  if (stripePendingId !== null && frameId !== stripePendingId && stripeDirty &&
      !(stripeDrained() && stripeClock.settled())) {
    stripePresent();
  }
  stripePendingId = frameId;
}

function stripeMaybePresent() {
  if (!stripeDirty) return;
  if (stripeDrained() && (stripeBottom || stripeClock.settled())) { stripePresent(); return; }
  stripeScheduleSettle();
}

// Draws one decoded stripe at its row offset; always consumes the image.
function stripeCompose(image, y, h, frameId) {
  const w = image.displayWidth !== undefined ? image.displayWidth : image.width;
  if (!stripeEnsureBack(w, y + h)) { try { image.close(); } catch (err) {} return; }
  stripeBoundary(frameId);
  try { stripeBackCtx.drawImage(image, 0, y); stripeDirty = true; } catch (err) {}
  try { image.close(); } catch (err) {}
  if (stripeGeomKnown && y + h >= stripeGeomH) stripeBottom = true;
  stripeMaybePresent();
}

function onStripeError(y) {
  const info = stripeDecs[y];
  if (info) {
    try { if (info.dec.state !== 'closed') info.dec.close(); } catch (err) {}
    delete stripeDecs[y];
  }
  const now = Date.now();
  if (now - stripeSoftWindowStart > 10000) { stripeSoftWindowStart = now; stripeSoftErrors = 0; }
  // A burst of failures says decode should leave the worker for the page ladder.
  if (++stripeSoftErrors > 12) { self.postMessage({ type: 'decoderError' }); return; }
  sendNeedKey('stripe_error');
}

function onH264Stripe(buffer) {
  if (buffer.byteLength < 11) return;
  const head = new Uint8Array(buffer, 0, 10);
  const key = head[1] === 0x01;
  const frameId = (head[2] << 8) | head[3];
  const y = (head[4] << 8) | head[5];
  const w = (head[6] << 8) | head[7];
  const h = (head[8] << 8) | head[9];
  wireChunks++;
  if (frameId !== wireLastId) { wireFrames++; wireLastId = frameId; }
  stripeClock.note(frameId);
  const payload = buffer.slice(10);
  if (payload.byteLength === 0) return;
  let info = stripeDecs[y];
  let codec = info ? info.codec : null;
  if (key) {
    const parsed = parseAvcCodecFromAnnexB(new Uint8Array(payload));
    if (parsed) codec = parsed;
    else if (!codec) codec = wireHint;
  }
  if (!codec) { sendNeedKey('no_codec'); return; }
  if (!info || info.dec.state !== 'configured' || info.w !== w || info.h !== h || info.codec !== codec) {
    // Only a keyframe may (re)configure a row: deltas against a lost state are noise.
    if (!key) { sendNeedKey('no_key'); return; }
    if (info) { try { if (info.dec.state !== 'closed') info.dec.close(); } catch (err) {} }
    const rowY = y;
    const dec = new VideoDecoder({
      output: (f) => {
        const rowInfo = stripeDecs[rowY];
        const meta = rowInfo && rowInfo.meta.length ? rowInfo.meta.shift() : null;
        stripeCompose(f, rowY, f.displayHeight, meta ? meta.frameId : wireLastId);
      },
      error: () => onStripeError(rowY),
    });
    try {
      const cfg = { codec: codec, codedWidth: w, codedHeight: h, optimizeForLatency: true };
      if (wireSoftware) cfg.hardwareAcceleration = 'prefer-software';
      dec.configure(cfg);
    } catch (err) {
      try { if (dec.state !== 'closed') dec.close(); } catch (e2) {}
      onStripeError(y);
      return;
    }
    info = stripeDecs[y] = { dec: dec, w: w, h: h, codec: codec, gotKey: false, meta: [] };
  }
  if (!key && !info.gotKey) { sendNeedKey('no_key'); return; }
  if (!key && info.dec.decodeQueueSize > STRIPE_DECODE_QUEUE_LIMIT) {
    // Deltas behind a backlog only deepen it; the row is gated until its next IDR.
    info.gotKey = false;
    sendNeedKey('overload');
    return;
  }
  try {
    info.meta.push({ frameId: frameId });
    info.dec.decode(new EncodedVideoChunk({ type: key ? 'key' : 'delta', timestamp: performance.now() * 1000, data: payload }));
    if (key) info.gotKey = true;
  } catch (err) {
    info.meta.pop();
    onStripeError(y);
  }
}

function onJpegStripe(buffer) {
  if (buffer.byteLength < 7) return;
  const head = new Uint8Array(buffer, 0, 6);
  const frameId = (head[2] << 8) | head[3];
  const y = (head[4] << 8) | head[5];
  wireChunks++;
  if (frameId !== wireLastId) { wireFrames++; wireLastId = frameId; }
  stripeClock.note(frameId);
  const data = buffer.slice(6);
  if (data.byteLength === 0) return;
  const done = (image) => {
    jpegPending--;
    if (!image) { stripeMaybePresent(); return; }
    const last = jpegLastRowId[y];
    if (last !== undefined) {
      const behindBy = (last - frameId) & 0xFFFF;
      if (behindBy > 0 && behindBy <= JPEG_STRIPE_REORDER_WINDOW) {
        try { image.close(); } catch (err) {}
        stripeMaybePresent();
        return;
      }
    }
    jpegLastRowId[y] = frameId;
    const h = image.displayHeight !== undefined ? image.displayHeight : image.height;
    jpegRowHeights[y] = h;
    stripeCompose(image, y, h, frameId);
  };
  if (typeof ImageDecoder !== 'undefined') {
    let d = null;
    try { d = new ImageDecoder({ data: data, type: 'image/jpeg' }); }
    catch (err) { return; }
    jpegPending++;
    d.decode().then((r) => { try { d.close(); } catch (err) {} done(r.image); })
      .catch(() => { try { d.close(); } catch (err) {} done(null); });
  } else if (typeof createImageBitmap === 'function') {
    jpegPending++;
    createImageBitmap(new Blob([data], { type: 'image/jpeg' })).then(done).catch(() => done(null));
  }
}

function onWire(buffer) {
  if (!(buffer instanceof ArrayBuffer) || buffer.byteLength < 1) return;
  if (stripedOn) {
    const type = new Uint8Array(buffer, 0, 1)[0];
    if (type === 0x03) onJpegStripe(buffer);
    else if (type === 0x04) onH264Stripe(buffer);
    return;
  }
  if (buffer.byteLength < 11) return;
  const head = new Uint8Array(buffer, 0, 10);
  const key = head[1] === 0x01;
  const frameId = (head[2] << 8) | head[3];
  const w = (head[6] << 8) | head[7];
  const h = (head[8] << 8) | head[9];
  wireChunks++;
  if (frameId !== wireLastId) { wireFrames++; wireLastId = frameId; }
  const payload = buffer.slice(10);
  let codec = wireCodec;
  if (key) {
    const parsed = parseAvcCodecFromAnnexB(new Uint8Array(payload));
    if (parsed) codec = parsed;
    else if (!codec) codec = wireHint;
  }
  if (!codec) { sendNeedKey('no_codec'); return; }
  if (!dec || dec.state !== 'configured' || codec !== wireCodec || w !== wireW || h !== wireH) {
    // Only a keyframe may (re)configure: deltas against a lost state are noise.
    if (!key) { sendNeedKey('no_key'); return; }
    if (!configureDecoder(codec, w, h, wireSoftware)) return;
    wireCodec = codec; wireW = w; wireH = h;
    self.postMessage({ type: 'wireDims', w: w, h: h });
  }
  decodeChunk(key, payload, performance.now() * 1000);
}

const stripedCaps = {
  stripedDecode: typeof VideoDecoder !== 'undefined',
  jpegDecode: typeof ImageDecoder !== 'undefined' || typeof createImageBitmap === 'function',
};
if (typeof VideoTrackGenerator !== 'undefined') {
  try {
    const g = new VideoTrackGenerator();
    writer = g.writable.getWriter();
    mode = 'vtg';
    self.postMessage(Object.assign({ type: 'mode', mode: 'vtg', track: g.track }, stripedCaps), [g.track]);
  } catch (e) { self.postMessage(Object.assign({ type: 'mode', mode: 'canvas' }, stripedCaps)); }
} else {
  self.postMessage(Object.assign({ type: 'mode', mode: 'canvas' }, stripedCaps));
}

self.onmessage = (e) => {
  const m = e.data;
  if (m.canvas) { oc = m.canvas; ctx = oc.getContext('2d', { desynchronized: true }); if (!mode) mode = 'canvas'; return; }
  if (m.type === 'decoderConfig') { configureDecoder(m.codec, m.codedWidth, m.codedHeight, m.software); return; }
  if (m.type === 'closeDecoder') { closeDecoder(); return; }
  if (m.type === 'chunk') {
    // Not ready yet; the page will resend a keyframe.
    decodeChunk(m.key, m.data, m.timestamp);
    return;
  }
  if (m.type === 'wireIn') {
    if (m.codecHint) wireHint = m.codecHint;
    wireSoftware = !!m.software;
    wirePort = m.port;
    m.port.onmessage = (ev) => onWire(ev.data);
    if (!wireStatsTimer) {
      wireStatsTimer = setInterval(() => {
        if (!wireChunks && !presentedFrames) return;
        const rows = {};
        if (stripedOn) {
          for (const y in stripeDecs) rows[y] = stripeDecs[y].h;
          for (const y in jpegRowHeights) if (!(y in rows)) rows[y] = jpegRowHeights[y];
        } else if (dec && wireH > 0) {
          rows[0] = wireH;
        }
        self.postMessage({ type: 'wireStats', chunks: wireChunks, frames: wireFrames, lastId: wireLastId, presents: presentedFrames, rows: rows });
        wireChunks = 0; wireFrames = 0; presentedFrames = 0;
      }, 1000);
    }
    return;
  }
  if (m.type === 'wireMode') {
    const striped = !!m.striped;
    if (striped !== stripedOn) {
      stripedOn = striped;
      // The sink re-announces in the new mode, so the page re-hides its
      // canvas even when the old mode had already consumed 'presented'.
      presented = false;
      if (striped) closeDecoder(); else closeStripeState();
    }
    return;
  }
  if (m.type === 'wireGeom') {
    const know = m.w > 0 && m.h > 0;
    if (know && (m.w !== stripeGeomW || m.h !== stripeGeomH)) {
      stripeGeomW = m.w; stripeGeomH = m.h;
      if (stripeBack && (stripeBack.width !== m.w || stripeBack.height !== m.h)) {
        // A real geometry change; the server keyframes it, so stale rows go.
        stripeBack = null; stripeBackCtx = null;
        stripeDirty = false; stripePendingId = null;
        jpegLastRowId = {}; jpegRowHeights = {};
      }
    }
    stripeGeomKnown = stripeGeomKnown || know;
    return;
  }
  if (m.type === 'wireHints') {
    if (m.codecHint) wireHint = m.codecHint;
    if (m.software !== undefined) wireSoftware = !!m.software;
    return;
  }
  // Fallback: a main-thread-decoded frame transferred in.
  if (m.frame) {
    present(m.frame);
    ack();
  }
};`;

/**
 * Creates the main-thread (Chromium) track generator; the worker-only
 * VideoTrackGenerator is handled by the video worker instead.
 * @returns {{track: MediaStreamTrack, writable: WritableStream}|null}
 */
function createVideoTrackGenerator() {
  try {
    if (typeof MediaStreamTrackGenerator !== 'undefined') {
      const g = new MediaStreamTrackGenerator({ kind: 'video' });
      return { track: g, writable: g.writable };
    }
  } catch (e) {
    console.warn('MediaStreamTrackGenerator unavailable, using canvas:', e);
  }
  return null;
}

/**
 * Lazily wires the `<video>` element to a fresh track generator; a writable
 * that later errors or closes falls back to the canvas so the element never freezes.
 * @returns {boolean} True when the writer is ready.
 */
function ensureMstgWriter() {
  if (videoFrameWriter) return true;
  if (!videoElement) return false;
  const gen = createVideoTrackGenerator();
  if (!gen) return false;
  videoTrack = gen.track;
  try { videoFrameWriter = gen.writable.getWriter(); }
  catch (e) { console.warn('track writer failed:', e); try { videoTrack.stop(); } catch (_) {} videoTrack = null; return false; }
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

/** Closes the track generator writer and detaches its stream from the `<video>`. */
function teardownMstgWriter() {
  if (videoFrameWriter) { try { videoFrameWriter.close(); } catch (e) {} videoFrameWriter = null; }
  if (videoTrack) { try { videoTrack.stop(); } catch (e) {} videoTrack = null; }
  if (videoElement) { try { videoElement.srcObject = null; } catch (e) {} }
}

/**
 * Presents a VideoFrame through the main-thread track generator, showing the
 * `<video>` and hiding the canvas once it has rendered. Until then the frame
 * is also painted on the canvas, since a fresh connection has nothing there
 * yet and an empty `<video>` would show black. The resize handlers re-show
 * the canvas with a fresh transform, so it is re-hidden every frame and its
 * box re-mirrored whenever it changed; a backpressured sink drops the frame
 * rather than building latency.
 * @param {VideoFrame} frame
 * @returns {boolean} True when consumed (the caller must not close it), false
 *     to fall back to the canvas.
 */
function presentFrameToVideo(frame) {
  if (!ensureMstgWriter()) return false;
  if (!mstgActive) {
    mstgActive = true;
    mstgLastGeom = null;
    mstgRendered = false;
    // Only one sink may be shown. The worker canvas holds the last composite of
    // the striped mode just left, and it would cover this <video> until a reload.
    if (videoWorkerActive && videoWorkerMode !== 'vtg' && videoWorkerCanvas) {
      videoWorkerActive = false;
      videoWorkerRendered = false;
      videoWorkerCanvas.style.display = 'none';
    }
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
        // Rendering cannot be observed here; assume the frame was presented.
        mstgRendered = true;
      }
    }
  }
  if (canvas && videoElement) {
    if (mstgRendered && canvas.style.display !== 'none') canvas.style.display = 'none';
    if (canvasGeomDirty || mstgLastGeom === null) {
      mstgLastGeom = canvas.style.cssText;
      videoElement.style.cssText = mstgLastGeom;
      videoElement.style.display = 'block';
      videoElement.style.objectFit = 'fill';
      canvasGeomDirty = false;
    }
  }
  if (!mstgRendered && canvas && canvasContext && canvas.width > 0 && canvas.height > 0) {
    try { canvasContext.drawImage(frame, 0, 0); } catch (e) {}
  }
  if (videoFrameWriter.desiredSize !== null && videoFrameWriter.desiredSize <= 0) {
    frame.close();
    if (++mstgConsecutiveDrops >= SINK_STALL_DROP_LIMIT) {
      console.warn(`Video track sink stalled (${mstgConsecutiveDrops} consecutive drops); rebuilding it.`);
      deactivateMstg();
    }
    return true;
  }
  mstgConsecutiveDrops = 0;
  const activeWriter = videoFrameWriter;
  videoFrameWriter.write(frame).catch(() => {
    try { frame.close(); } catch (e) {}
    if (videoFrameWriter === activeWriter) deactivateMstg();
  });
  return true;
}

/**
 * Tells the video worker the display geometry, which sizes the back-buffer
 * its striped composite persists on; the canvas buffer is stream-resolution
 * physical pixels, exactly the space stripe offsets address.
 * @returns {void}
 */
function syncWireGeom() {
  if (!videoWorker || !canvas || !(canvas.width > 0) || !(canvas.height > 0)) return;
  try { videoWorker.postMessage({ type: 'wireGeom', w: canvas.width, h: canvas.height }); }
  catch (e) { /* respawns fresh */ }
}

/**
 * Connects the socket worker to the video worker, so diverted stripes reach
 * decode without the page's thread on them; a fresh channel per call, for a
 * restarted transport or video worker. The divert itself stays off until
 * updateVideoDivert turns it on.
 * @returns {void}
 */
function wireSocketToVideoWorker() {
  if (!videoWorker || !websocket || typeof websocket.connectVideo !== 'function') return;
  const channel = new MessageChannel();
  try {
    videoWorker.postMessage({
      type: 'wireIn', port: channel.port1,
      codecHint: workerKeyframeCodec, software: preferSoftwareDecode,
    }, [channel.port1]);
    websocket.connectVideo(channel.port2);
  } catch (e) {
    console.warn('Could not connect the socket worker to the video worker:', e);
  }
  syncWireGeom();
  updateVideoDivert(true);
}

/**
 * Points the socket worker's divert at the current truth: on while the video
 * worker is the decoder for `h264enc` and healthy, or, in the striped modes,
 * while it can decode and composite them (any sink; gated off with the
 * worker escape hatch). Called wherever any input flips -- the worker's mode
 * reply, its deactivation or decoder failure, an encoder switch, a rebuilt
 * transport. Striped acks report the presented id, so a client that cannot
 * render sheds load exactly as the page path does.
 * @param {boolean} [force] Resend even when the value is unchanged, for a
 *     transport whose worker started fresh and holds the default.
 * @returns {void}
 */
function updateVideoDivert(force) {
  const striped = currentEncoderMode === 'h264enc-striped' || currentEncoderMode === 'jpeg';
  const stripedReady = USE_OFFSCREEN_WORKER && videoWorkerReady &&
    (currentEncoderMode === 'jpeg' ? videoWorkerJpegDecode : videoWorkerStripedDecode);
  const on = !!(websocket && typeof websocket.setVideoDivert === 'function' &&
    videoWorkerReady && !workerDecodeFailed &&
    (striped ? stripedReady : (decodeInWorker && currentEncoderMode === 'h264enc')));
  if (videoWorker) {
    try { videoWorker.postMessage({ type: 'wireMode', striped }); } catch (e) { /* respawns fresh */ }
  }
  // The striped flag is part of the state: a full-frame divert rolling into a
  // striped one keeps `on` but changes the ack source and the sink upkeep.
  if (on === videoDivertOn && striped === videoDivertStriped && !force) return;
  videoDivertOn = on;
  videoDivertStriped = striped;
  window.videoDivertOn = on;
  if (!on) window.videoStripeRows = {};
  if (websocket && typeof websocket.setVideoDivert === 'function') {
    websocket.setVideoDivert(on, striped ? 'present' : 'receive');
  }
  // The diverted wire feeds the worker sink directly, so the page-side present
  // helpers that would normally reveal it never run; shown from here instead.
  if (on) {
    if (striped) {
      // The worker decodes now; page-built row decoders would only sit
      // configured behind it until the next resize or reconnect.
      clearAllVncStripeDecoders();
      cleanupJpegStripeQueue();
    }
    syncWireGeom();
    activateWorkerSinkDisplay();
  }
}

/** Sink descriptions already announced, so a re-handshake never repeats one. */
const announcedSinks = new Set();
/**
 * Says where frames are being presented, once per distinct sink. Every path
 * that settles presentation announces through this, including the fall back to
 * the page canvas when a worker sink fails: a session whose sink is not the one
 * it asked for says so as plainly as one that got it.
 * @param {string} description
 */
function announceSink(description) {
  if (announcedSinks.has(description)) return;
  announcedSinks.add(description);
  console.info(`[Selkies] video sink: ${description}`);
}

/**
 * Lazily creates the video worker and completes its capability handshake.
 * The worker self-probes VideoTrackGenerator on startup and reports `vtg`
 * (it transferred a track back for `<video>.srcObject`) or `canvas` (it is
 * handed an OffscreenCanvas to composite on). Its other messages: `ack` per
 * consumed frame, `error` when the generator writable failed, `presented`
 * once its canvas has real content, `needKeyframe` (`no_key` after a
 * reconfigure, `overload` when the decode backlog forced a resync; throttled
 * to one per 800 ms) and `decoderError`, after which chunks return to
 * main-thread decode while the sink stays up for transferred frames.
 * @returns {boolean} True once a sink is wired; until then frames fall back to
 *     the main canvas.
 */
function ensureVideoWorker() {
  if (videoWorkerSinkInert) return false;
  if (videoWorkerReady) return true;
  if (videoWorker) return false;
  try {
    // The Worker keeps its own reference to the script, so the object URL is
    // revoked at once rather than living until the page unloads.
    const workerURL = URL.createObjectURL(new Blob([VIDEO_WORKER_SRC], { type: 'text/javascript' }));
    videoWorkerSpawnAttempts++;
    videoWorker = new Worker(workerURL);
    URL.revokeObjectURL(workerURL);
    videoWorkerInFlight = 0;
    wireSocketToVideoWorker();
    videoWorker.onerror = (e) => { console.warn('video worker error:', e && e.message); deactivateVideoWorker(); };
    videoWorker.onmessage = (e) => {
      const m = e.data;
      if (!m) return;
      if (m.ack) { if (videoWorkerInFlight > 0) videoWorkerInFlight--; return; }
      if (m.type === 'error') { deactivateVideoWorker(); return; }
      if (m.type === 'presented') {
        videoWorkerRendered = true;
        if (videoWorkerActive && canvas) canvas.style.display = 'none';
        return;
      }
      if (m.type === 'needKeyframe') {
        console.info(`[VideoWorker] keyframe requested: ${m.reason}`);
        requestKeyframe();
        return;
      }
      if (m.type === 'decoderError') {
        workerDecodeFailed = true;
        workerDecoderCodec = null; workerDecoderW = 0; workerDecoderH = 0;
        workerKeyframeCodec = null;
        updateVideoDivert();
        return;
      }
      if (m.type === 'wireStats') {
        window.videoChunksReceived += m.chunks;
        const stripedNow = currentEncoderMode === 'h264enc-striped' || currentEncoderMode === 'jpeg';
        // Striped fps means composites presented, exactly as on the page path;
        // full-frame rate stays a wire measure.
        if (stripedNow) frameCount += (m.presents || 0);
        else divertedWireFramesThisPeriod += m.frames;
        if (m.lastId >= 0) { lastReceivedVideoFrameId = m.lastId; lastReceivedVideoFrameAt = performance.now(); }
        if (m.rows) window.videoStripeRows = m.rows;
        if (isSharedMode && m.chunks > 0) lastSharedVideoChunkTime = performance.now();
        if (!streamStarted && (m.frames > 0 || (m.presents || 0) > 0)) startStream();
        if (videoDivertOn) {
          activateWorkerSinkDisplay();
          checkWorkerSinkAlive();
        }
        return;
      }
      if (m.type === 'wireDims') {
        // A shared viewer's geometry comes from the wire; diverted, the wire
        // is only visible here.
        if (isSharedMode && m.w > 0 && m.h > 0 &&
            (manual_width !== m.w || manual_height !== m.h)) {
          manual_width = m.w;
          manual_height = m.h;
          console.log(`Shared mode: stream is ${manual_width}x${manual_height} (physical).`);
          applyManualCanvasStyle(manual_width, manual_height, true);
        }
        return;
      }
      if (m.type === 'mode') {
        videoWorkerStripedDecode = !!m.stripedDecode;
        videoWorkerJpegDecode = !!m.jpegDecode;
        if (m.mode === 'vtg' && m.track) {
          if (!videoElement) { deactivateVideoWorker(); return; }
          videoWorkerMode = 'vtg';
          videoWorkerTrack = m.track;
          try {
            videoElement.srcObject = new MediaStream([m.track]);
            const p = videoElement.play(); if (p && p.catch) p.catch(() => {});
          } catch (err) { console.warn('VTG srcObject failed:', err); deactivateVideoWorker(); return; }
          announceSink('VideoTrackGenerator in the video worker.');
          decodeInWorker = true;
          videoWorkerReady = true;
          videoWorkerSpawnAttempts = 0;
          syncWireGeom();
          updateVideoDivert();
        } else if (supportsWindowMSTG &&
            currentEncoderMode !== 'h264enc-striped' && currentEncoderMode !== 'jpeg') {
          // No worker generator, but the page has one: it feeds a <video>
          // element, which beats compositing a canvas by hand.
          announceSink('MediaStreamTrackGenerator on the page — '
            + 'this browser exposes no VideoTrackGenerator to a worker.');
          deactivateVideoWorker();
          return;
        } else {
          videoWorkerMode = 'canvas';
          announceSink(supportsWindowMSTG
            ? 'OffscreenCanvas in the video worker for the striped '
              + 'codecs; full-frame H.264 presents through the page MediaStreamTrackGenerator.'
            : 'OffscreenCanvas in the video worker — '
              + 'this browser exposes no VideoTrackGenerator to a worker and no '
              + 'MediaStreamTrackGenerator on the page, so frames are composited rather '
              + 'than handed to a <video> element.');
          if (!videoWorkerCanvas) { deactivateVideoWorker(); return; }
          try {
            const off = videoWorkerCanvas.transferControlToOffscreen();
            videoWorkerCanvasTransferred = true;
            videoWorker.postMessage({ canvas: off }, [off]);
          } catch (err) { console.warn('OffscreenCanvas transfer failed:', err); deactivateVideoWorker(); return; }
          // With a page generator the worker serves only the striped divert;
          // full-frame decode stays on the page feeding that generator.
          decodeInWorker = !supportsWindowMSTG;
          videoWorkerReady = true;
          videoWorkerSpawnAttempts = 0;
          syncWireGeom();
          updateVideoDivert();
        }
      }
    };
    return false;
  } catch (e) {
    console.warn('video worker init failed, using main canvas:', e);
    deactivateVideoWorker();
    return false;
  }
}

/**
 * Terminates the video worker and returns presentation to the main canvas.
 * The worker decoder config is forgotten so a recreated worker is configured
 * afresh, and a transferred OffscreenCanvas, which can never be transferred
 * again, is replaced by a fresh `<canvas>` element. The sink is announced from
 * here as well: this is where a worker that failed to start or died hands
 * presentation back, and a session that ends up somewhere other than where it
 * asked to be is the one that most needs to say so.
 */
function deactivateVideoWorker() {
  const wasVtg = (videoWorkerMode === 'vtg');
  announceSink(supportsWindowMSTG
    ? 'MediaStreamTrackGenerator on the page.'
    : '2D canvas on the page — no video worker sink.');
  const wasTransferred = videoWorkerCanvasTransferred;
  videoWorkerActive = false; videoWorkerReady = false; videoWorkerMode = null;
  decodeInWorker = false;
  videoWorkerStripedDecode = false; videoWorkerJpegDecode = false;
  workerSinkInertSince = 0;
  updateVideoDivert();
  videoWorkerInFlight = 0; videoWorkerCanvasTransferred = false;
  videoWorkerRendered = false; sinkRevealGen++;
  workerDecoderCodec = null; workerDecoderW = 0; workerDecoderH = 0;
  workerKeyframeCodec = null;
  if (videoWorker) { try { videoWorker.terminate(); } catch (_) {} videoWorker = null; }
  if (wasVtg) {
    if (videoWorkerTrack) { try { videoWorkerTrack.stop(); } catch (_) {} videoWorkerTrack = null; }
    if (videoElement) { try { videoElement.srcObject = null; } catch (_) {} videoElement.style.display = 'none'; }
  }
  if (wasTransferred && videoWorkerCanvas) {
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

/**
 * Shows the active worker sink (`<video>` for VTG, the worker canvas
 * otherwise), hides the main canvas once the sink has rendered
 * (requestVideoFrameCallback for VTG, the worker's one-time `presented`
 * message for canvas mode), and mirrors the canvas box onto the sink whenever
 * it changed.
 * @returns {boolean} False while no sink target exists yet.
 */
function activateWorkerSinkDisplay() {
  const target = (videoWorkerMode === 'vtg') ? videoElement : videoWorkerCanvas;
  if (!target) return false;
  if (!videoWorkerActive) {
    videoWorkerActive = true; videoWorkerLastGeom = null;
    videoWorkerRendered = false;
    // The page generator's <video> is the other sink; shown, it would cover
    // this canvas with the last frame of the mode just left.
    if (target !== videoElement) {
      if (mstgActive) deactivateMstg();
      else if (videoElement) videoElement.style.display = 'none';
    }
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
        // Rendering cannot be observed here; assume the frame was presented.
        videoWorkerRendered = true;
      }
    }
  }
  if (canvas) {
    if (videoWorkerRendered && canvas.style.display !== 'none') canvas.style.display = 'none';
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

/**
 * Transfers a main-thread-decoded VideoFrame to the worker sink, the fallback
 * while the worker decoder warms up. A frame past the in-flight cap is dropped
 * rather than queued behind a stalled decoder, and a frame that postMessage
 * detached or closed is reported consumed so the caller never reuses it.
 * @param {VideoFrame} frame
 * @returns {boolean} True when consumed (the caller must not close it).
 */
function presentFrameToWorker(frame) {
  if (!ensureVideoWorker()) return false;
  if (!activateWorkerSinkDisplay()) return false;
  checkWorkerSinkAlive();
  if (videoWorkerSinkInert) return false;
  if (videoWorkerInFlight >= VIDEO_WORKER_MAX_IN_FLIGHT) {
    try { frame.close(); } catch (_) {}
    return true;
  }
  try {
    videoWorker.postMessage({ frame }, [frame]);
    videoWorkerInFlight++;
  }
  catch (e) { try { frame.close(); } catch (_) {} deactivateVideoWorker(); return true; }
  return true;
}

let workerCfgLogLast = Number.NEGATIVE_INFINITY, workerCfgLogSuppressed = 0;
const WORKER_CFG_LOG_MIN_INTERVAL_MS = 5000;
/**
 * Rate-limited log of worker decoder reconfigures. A healthy stream
 * reconfigures about once per session (join, resolution change), so a storm
 * with flipping codec strings is the diagnostic; at most one line per
 * interval, with a suppressed count so repeats stay visible.
 * @param {string} codec
 * @param {number} w
 * @param {number} h
 */
function logWorkerDecoderConfig(codec, w, h) {
  const now = performance.now();
  if (now - workerCfgLogLast >= WORKER_CFG_LOG_MIN_INTERVAL_MS) {
    const suppressed = workerCfgLogSuppressed > 0 ? ` (+${workerCfgLogSuppressed} suppressed)` : '';
    console.info(`[VideoWorker] decoder (re)configure: codec=${codec} ${w}x${h}${workerDecoderCodec ? ` (was ${workerDecoderCodec} ${workerDecoderW}x${workerDecoderH})` : ''}${suppressed}`);
    workerCfgLogLast = now;
    workerCfgLogSuppressed = 0;
  } else {
    workerCfgLogSuppressed++;
  }
}

/**
 * Forwards an encoded full-frame H.264 chunk to the worker's own decoder,
 * reconfiguring it when the codec or coded dimensions change and requesting
 * the keyframe WebCodecs needs after a configure.
 * @param {boolean} isKey
 * @param {ArrayBuffer} dataBuf The Annex-B payload; transferred, not copied.
 * @param {number} w Coded width.
 * @param {number} h Coded height.
 * @param {string} codec The `avc1.PPCCLL` codec string.
 * @returns {boolean} True when handled there, false to fall back to main-thread decode.
 */
function feedWorkerDecoder(isKey, dataBuf, w, h, codec) {
  if (workerDecodeFailed) return false;
  if (!ensureVideoWorker()) return false;
  if (!activateWorkerSinkDisplay()) return false;
  if (codec !== workerDecoderCodec || w !== workerDecoderW || h !== workerDecoderH) {
    logWorkerDecoderConfig(codec, w, h);
    try { videoWorker.postMessage({ type: 'decoderConfig', codec: codec, codedWidth: w, codedHeight: h, software: preferSoftwareDecode }); }
    catch (e) { return false; }
    workerDecoderCodec = codec; workerDecoderW = w; workerDecoderH = h;
    requestKeyframe();
  }
  try { videoWorker.postMessage({ type: 'chunk', key: isKey, data: dataBuf, timestamp: performance.now() * 1000 }, [dataBuf]); }
  catch (e) { return false; }
  return true;
}

/** Returns presentation from the main-thread track generator to the canvas; idempotent. */
function deactivateMstg() {
  if (!mstgActive) return;
  mstgActive = false;
  mstgConsecutiveDrops = 0;
  mstgRendered = false; sinkRevealGen++;
  if (videoElement) videoElement.style.display = 'none';
  if (canvas) canvas.style.display = '';
  teardownMstgWriter();
}

/**
 * Pre-stream guess of the H.264 codec string. Decoder creation re-derives the
 * exact codec from the first keyframe's SPS (codecFromKeyframe), and outside
 * Chromium only a conservative baseline is guessed because Safari rejects a
 * stream whose real profile or level exceeds the configured one.
 * @param {number} width
 * @param {number} height
 * @param {boolean} is444 Whether the stream is 4:4:4 full-color.
 * @param {number} fps
 * @returns {string}
 */
const getDynamicH264Codec = (width, height, is444, fps) => {
  if (!isChromium) {
    return 'avc1.42E01E';
  }
  const effFps = (typeof fps === 'number' && fps > 0) ? fps : 60;
  const pixelsPerSecond = width * height * effFps;
  // NVENC's emitted profile_idc: High (0x64) for 4:2:0, High 4:4:4 (0xF4) for 4:4:4.
  const profile = is444 ? 'F400' : '6400';
  // Floored at level 5.2 (0x34), the encoder's emitted level, so the first
  // keyframe does not trigger a level-only reconfigure.
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


/**
 * The H.264 codec string a keyframe's in-band SPS declares. Every engine uses
 * this: Safari's VideoDecoder errors when the configured profile or level is
 * lower than the stream's real one, and the parsed value always matches the
 * bitstream.
 * @param {ArrayBuffer|Uint8Array} keyframeBytes
 * @param {string} fallback Used when no SPS can be read.
 * @returns {string}
 */
const codecFromKeyframe = (keyframeBytes, fallback) => {
  if (!keyframeBytes || !keyframeBytes.byteLength) return fallback;
  try {
    const parsed = parseAvcCodecFromAnnexB(
      keyframeBytes instanceof Uint8Array ? keyframeBytes : new Uint8Array(keyframeBytes));
    return parsed || fallback;
  } catch (_) {
    return fallback;
  }
};

/**
 * Picks the canvas `image-rendering`: pixelated for a 1:1 display or when
 * anti-aliasing is off, smoothed whenever the picture is scaled (manual
 * resolution, high-DPR CSS scaling, shared mode). Part of cssText, so the box
 * is re-mirrored to the active sink.
 */
const updateCanvasImageRendering = () => {
  if (!canvas) return;
  canvasGeomDirty = true;
  if (!antiAliasingEnabled) {
    if (canvas.style.imageRendering !== 'pixelated') {
      console.log("Anti-aliasing disabled by setting. Forcing 'pixelated' rendering.");
      canvas.style.imageRendering = 'pixelated';
      canvas.style.setProperty('image-rendering', 'crisp-edges', '');
    }
    return;
  }
  const dpr = window.devicePixelRatio || 1;
  if (isSharedMode || window.manual_resolution || (useCssScaling && dpr > 1)) {
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

/** Installs the page's base stylesheet: the video container, its sinks, the overlay input and the start button. */
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

/**
 * Settles whether full colour is on the table for this engine, before the
 * first SETTINGS payload is built.
 *
 * An engine whose decoder has no 4:4:4 profile cannot show a full-colour
 * stream at all -- every stripe is refused and nothing paints -- so the
 * setting is turned off rather than asked for. It is written to storage, not
 * merely dropped from one payload: every payload is built from storage, and
 * the dashboards read the same keys, so the toggle shows what the stream is.
 */
async function settleFullColorSupport() {
    if (await canDecodeFullColor()) return;
    if (!getBoolParam('video_fullcolor', false)) return;
    console.warn('[Selkies] full colour (4:4:4) is off: this browser decodes H.264 4:2:0 only.');
    video_fullcolor = false;
    setBoolParam('video_fullcolor', false);
}

let codecRefusalAnswered = false;
let codecRefusalUnanswerable = false;
/**
 * Answers a decoder config this engine will not take, and reports it.
 *
 * The codec is the stream's own, read from the keyframe's SPS, so it is what
 * the server is really sending rather than what the settings say: a
 * `video_fullcolor` the server holds locked arrives as 4:4:4 in the bitstream
 * and as nothing at all in the settings echo. Left alone, every stripe of
 * every frame builds a decoder and has it refused, and the page stays black
 * without saying why.
 *
 * A client that owns its settings takes the ladder's own last rung, the JPEG
 * encoder, whose stripes need no `VideoDecoder`. Refusals that outlive that
 * switch -- a decoder refused again, or an H.264 keyframe still arriving on
 * the page path -- mean the server holds the encoder too, and a shared viewer
 * owns none of the stream's settings to begin with: both are told, once.
 * @param {string} codec The refused codec string.
 */
function answerRefusedCodec(codec) {
    if (codecRefusalUnanswerable) return;
    if (!codecRefusalAnswered) {
        codecRefusalAnswered = true;
        if (!isSharedMode && currentEncoderMode !== 'jpeg') {
            console.warn(`This browser has no decoder for ${codec}; switching to the JPEG encoder.`);
            pinJpegEncoder();
            sendFullSettingsUpdateToServer(`no decoder for ${codec}`);
            return;
        }
    }
    codecRefusalUnanswerable = true;
    console.error(`This session streams ${codec}, which this browser cannot decode.`);
    if (statusDisplayElement) {
        statusDisplayElement.textContent = 'Error: This session streams video in a format this '
            + 'browser cannot decode. Full color (4:4:4) needs a browser whose decoder has that profile.';
        statusDisplayElement.classList.remove('hidden');
    }
}

/**
 * Sends the full `SETTINGS,{json}` payload; never from a shared viewer.
 * @param {string} reason Logged with the send.
 */
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

/**
 * Builds the SETTINGS payload. Only keys with a stored (user-set) value are
 * included, so the fallbacks here never override server-configured defaults
 * for an untouched setting; `scaling_dpi` is the exception, being
 * client-authoritative (the derived default or the dashboard's pick, sent
 * live so it reaches the running server; the desktop DPI is independent of
 * the resolution). The payload also carries the keyboard layout, the client
 * geometry or manual resolution, the display identity and the audio-RED
 * capability that makes the server enable Opus redundancy.
 * @returns {Object<string, *>}
 */
/**
 * This page's remote pixels per CSS pixel, reported so a neighboring display
 * can scale a cross-display drag's travel over this one: the presented stream
 * box where one is measurable (manual mode scales it freely), else the device
 * pixel ratio the resolution request was built with.
 * @param {number} dpr
 * @returns {number}
 */
function currentDisplayScale(dpr) {
    const canvas = document.getElementById('videoCanvas');
    if (canvas && canvas.width > 0) {
        const sinks = [canvas, document.getElementById('videoStream'),
                       document.getElementById('videoWorkerCanvas')];
        for (const el of sinks) {
            const r = el && el.getBoundingClientRect ? el.getBoundingClientRect() : null;
            if (r && r.width > 0) return canvas.width / r.width;
        }
    }
    return dpr;
}

function getCurrentSettingsPayload() {
    const settingsToSend = {};
    const dpr = useCssScaling ? 1 : (window.devicePixelRatio || 1);
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
        ['manual_resolution', () => getBoolParam('manual_resolution', false)],
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
        ['video_bitrate', () => getIntParam('video_bitrate', 8000)],
        ['force_aligned_resolution', () => getBoolParam('force_aligned_resolution', false)],
    ];
    for (const [key, read] of storedEntries) {
        if (hasStoredParam(key)) settingsToSend[key] = read();
    }
    settingsToSend['scaling_dpi'] = scalingDPI;
    if (detectedKeyboardLayout) {
        settingsToSend['keyboardLayout'] = detectedKeyboardLayout;
    }
    if (window.manual_resolution && manual_width != null && manual_height != null) {
        settingsToSend['manual_resolution'] = true;
        settingsToSend['manual_width'] = alignResolution(manual_width);
        settingsToSend['manual_height'] = alignResolution(manual_height);
    } else {
        const videoContainer = document.querySelector('.video-container');
        const rect = videoContainer ? videoContainer.getBoundingClientRect() : { width: window.innerWidth, height: window.innerHeight };
        settingsToSend['manual_resolution'] = false;
        
        let initW = alignResolution(rect.width * dpr);
        let initH = alignResolution(rect.height * dpr);
        if (initW > 4080) initW = 4080;
        if (initH > 4080) initH = 4080;

        settingsToSend['initialClientWidth'] = initW;
        settingsToSend['initialClientHeight'] = initH;
    }
    settingsToSend['useCssScaling'] = useCssScaling;
    settingsToSend['displayId'] = displayId;
    settingsToSend['displayScale'] = currentDisplayScale(dpr);
    if (displayId === 'display2') {
        settingsToSend['displayPosition'] = displayPosition;
    }
    settingsToSend['audioRedundancy'] = true;
    return settingsToSend;
}

/**
 * Labels a pipeline toggle button with its name and ON/OFF state.
 * @param {HTMLElement|null} buttonElement
 * @param {boolean} isActive
 */
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

/**
 * Sends `r,WxH,displayId` with the aligned, DPR-scaled and 4080-capped stream
 * resolution; blocked in shared mode, where the viewer follows the controller.
 * @param {number} width CSS pixels, or the exact size in manual mode.
 * @param {number} height
 */
function sendResolutionToServer(width, height) {
  if (isSharedMode) {
    console.log("Shared mode: Resolution sending to server is blocked.");
    return;
  }

  let realWidth, realHeight;
  let dprUsed = 1;

  if (window.manual_resolution) {
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
  console.log(`Sending resolution to server: ${resString}, DisplayID: ${displayId}, Manual Mode: ${window.manual_resolution}, Pixel Ratio Used: ${dprUsed}, useCssScaling: ${useCssScaling}`);

  if (websocket && websocket.readyState === WebSocket.OPEN) {
    websocket.send(`r,${resString},${displayId}`);
  } else {
    console.warn("Cannot send resolution via WebSocket: Connection not open.");
  }
}

/**
 * Mirrors the canvas box onto the active video sink right after a canvas-style
 * writer rewrote it. The present paths do the same, but only when frames flow:
 * on a static remote a resize would otherwise leave the stale canvas covering
 * the live sink until the next decoded frame. A sink that has proven it
 * renders gets the geometry and hides the canvas immediately; during warm-up
 * nothing changes. Covers all three sinks (main-thread and worker generators
 * drive the `<video>`, the OffscreenCanvas worker drives `videoWorkerCanvas`).
 */
function syncSinkToCanvasStyle() {
  if (!canvas) return;
  let target = null, rendered = false, isMstg = false;
  if (mstgActive && videoElement) {
    target = videoElement;
    rendered = mstgRendered;
    isMstg = true;
  } else if (videoWorkerActive) {
    target = (videoWorkerMode === 'vtg') ? videoElement : videoWorkerCanvas;
    rendered = videoWorkerRendered;
  }
  if (!target) return;
  const geom = canvas.style.cssText;
  target.style.cssText = geom;
  target.style.display = 'block';
  target.style.objectFit = 'fill';
  if (isMstg) mstgLastGeom = geom; else videoWorkerLastGeom = geom;
  canvasGeomDirty = false;
  if (rendered) canvas.style.display = 'none';
}

/**
 * Sizes the canvas for a manual resolution: the backing buffer at the target
 * size (DPR-scaled unless CSS scaling, shared mode or manual mode pin it to
 * 1), the CSS box either scaled to fit the container or exact and centered.
 * The overlay input follows the box and the input handler is told to resize.
 * The per-row JPEG stripe ids, keyed by row offset, are reset because a
 * geometry change invalidates them.
 * @param {number} targetWidth
 * @param {number} targetHeight
 * @param {boolean} scaleToFit
 */
function applyManualCanvasStyle(targetWidth, targetHeight, scaleToFit) {
  if (!canvas || !canvas.parentElement) {
    console.error("Cannot apply manual canvas style: Canvas or parent container not found.");
    return;
  }
  if (targetWidth <=0 || targetHeight <=0) {
    console.warn(`Cannot apply manual canvas style: Invalid target dimensions ${targetWidth}x${targetHeight}`);
    return;
  }
  canvasGeomDirty = true;
  lastDrawnJpegStripeFrameId = {};

  const dpr = (isSharedMode || window.manual_resolution || useCssScaling) ? 1 : (window.devicePixelRatio || 1);
  const internalBufferWidth = alignResolution(targetWidth * dpr);
  const internalBufferHeight = alignResolution(targetHeight * dpr);

  if (canvas.width !== internalBufferWidth || canvas.height !== internalBufferHeight) {
    canvas.width = internalBufferWidth;
    canvas.height = internalBufferHeight;
    console.log(`Canvas internal buffer set to: ${internalBufferWidth}x${internalBufferHeight}`);
    syncWireGeom();
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
  syncSinkToCanvasStyle();

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

/**
 * Sizes the canvas for the stream's own resolution: the backing buffer at the
 * DPR-scaled size and the CSS box at the stream size, centered in the
 * container, with the overlay input following. The per-row JPEG stripe ids
 * are reset as in applyManualCanvasStyle.
 * @param {number} streamWidth
 * @param {number} streamHeight
 */
function resetCanvasStyle(streamWidth, streamHeight) {
  if (!canvas) return;
  if (streamWidth <= 0 || streamHeight <= 0) {
    console.warn(`Cannot reset canvas style: Invalid stream dimensions ${streamWidth}x${streamHeight}`);
    return;
  }
  lastDrawnJpegStripeFrameId = {};
  canvasGeomDirty = true;

  const dpr = useCssScaling ? 1 : (window.devicePixelRatio || 1); 
  const internalBufferWidth = alignResolution(streamWidth * dpr);
  const internalBufferHeight = alignResolution(streamHeight * dpr);

  if (canvas.width !== internalBufferWidth || canvas.height !== internalBufferHeight) {
    canvas.width = internalBufferWidth;
    canvas.height = internalBufferHeight;
    console.log(`Canvas internal buffer reset to: ${internalBufferWidth}x${internalBufferHeight}`);
    syncWireGeom();
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
  syncSinkToCanvasStyle();

  if (window.webrtcInput && typeof window.webrtcInput.resize === 'function') {
      window.webrtcInput.resize();
  }
}

/** Switches the window resize listener to the automatic (stream follows the viewport) handler and applies it once. */
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

/** Resize listener for manual resolution: restyles the canvas box without touching the stream size. */
const directManualLocalScalingHandler = () => {
  if (window.manual_resolution && !isSharedMode && manual_width != null && manual_height != null && manual_width > 0 && manual_height > 0) {
    applyManualCanvasStyle(manual_width, manual_height, scaleLocallyManual);
  }
};

/** Switches the window resize listener to the manual-resolution handler and applies it once. */
function disableAutoResize() {
  if (originalWindowResizeHandler) {
    console.log("Switching to Manual Mode Local Scaling: Removing original (auto) resize listener.");
    window.removeEventListener('resize', originalWindowResizeHandler);
  }
  console.log("Switching to Manual Mode Local Scaling: Adding direct manual scaling listener.");
  window.removeEventListener('resize', directManualLocalScalingHandler);
  window.addEventListener('resize', directManualLocalScalingHandler);
  if (window.manual_resolution && !isSharedMode && manual_width != null && manual_height != null && manual_width > 0 && manual_height > 0) {
    console.log("Applying current manual canvas style after enabling direct manual resize handler.");
    applyManualCanvasStyle(manual_width, manual_height, scaleLocallyManual);
  }
}

/** Marks the container as a shared viewer (default cursor) and disables file upload. */
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

/**
 * Builds the page: the video container with its status bar, overlay input,
 * canvas, the sink elements the engine can use, and the start button, plus
 * the hidden file input and keyboard-assist input on the body. Chooses the
 * video sink (see `supportsWindowMSTG`), logging it once since a canvas
 * fallback explains a session's CPU cost, and starts the worker handshake
 * early so its decoder is ready before the first frame, then sizes the canvas
 * for shared, manual or automatic resolution.
 */
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
  // Without these every tap on the overlay opens a mobile engine's soft keyboard
  // over the session; #keyboard-input-assist is what deliberately opens it.
  overlayInput.inputMode = 'none';
  overlayInput.virtualKeyboardPolicy = 'manual';
  overlayInput.setAttribute('autocorrect', 'off');
  overlayInput.setAttribute('autocapitalize', 'off');
  overlayInput.setAttribute('spellcheck', 'false');
  overlayInput.id = 'overlayInput';
  videoContainer.appendChild(overlayInput);

  canvas = document.getElementById('videoCanvas');
  if (!canvas) {
    canvas = document.createElement('canvas');
    canvas.id = 'videoCanvas';
  }
  videoContainer.appendChild(canvas);

  const offscreenWorkerUrlParam = urlParams.get('offscreen_worker');
  const offscreenWorkerEnabled = (offscreenWorkerUrlParam !== null)
    ? (offscreenWorkerUrlParam.toLowerCase() === 'true')
    : getBoolParam('offscreen_worker', true);
  // The worker is asked first because it is the only place VideoTrackGenerator
  // exists; a page-side MediaStreamTrackGenerator takes over only once it has
  // reported it has none (see the worker's mode reply in ensureVideoWorker).
  USE_OFFSCREEN_WORKER = offscreenWorkerEnabled;
  stripeCompositeEnabled = offscreenWorkerEnabled;
  if (!USE_OFFSCREEN_WORKER && supportsWindowMSTG) {
    announceSink('MediaStreamTrackGenerator on the page.');
  } else if (!USE_OFFSCREEN_WORKER) {
    announceSink('2D canvas on the page — '
      + (offscreenWorkerEnabled
          ? 'no MediaStreamTrackGenerator in this browser and no video worker.'
          : 'the video worker is disabled (offscreen_worker=false).')
      + ' Every frame is drawn by hand, which costs more CPU than a <video> sink.');
  }

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

  if (USE_OFFSCREEN_WORKER) {
    videoWorkerCanvas = document.getElementById('videoWorkerCanvas');
    if (!videoWorkerCanvas) {
      videoWorkerCanvas = document.createElement('canvas');
      videoWorkerCanvas.id = 'videoWorkerCanvas';
    }
    videoWorkerCanvas.style.display = 'none';
    videoContainer.appendChild(videoWorkerCanvas);
  }

  if (USE_OFFSCREEN_WORKER) ensureVideoWorker();

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
  } else if (manual_resolution && manual_width != null && manual_height != null && manual_width > 0 && manual_height > 0) {
    applyManualCanvasStyle(manual_width, manual_height, scaleLocallyManual);
    disableAutoResize();
    console.log(`Initialized UI in Manual Resolution Mode: ${manual_width}x${manual_height} (logical), ScaleLocally: ${scaleLocallyManual}`);
  } else {
    const initialStreamWidth = 1024;
    const initialStreamHeight = 768;
    resetCanvasStyle(initialStreamWidth, initialStreamHeight);
    console.log("Initialized UI in Auto Resolution Mode (defaulting to 1024x768 logical for now)");
  }
  // No readback happens on this canvas, so the low-latency hint has no downside.
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

/** Closes every stripe decoder and forgets their soft-error counts. */
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
  stripeDecodeSoftErrors = {};
  console.log("All VNC stripe decoders and metadata cleared.");
}

/** Window within which repeated soft errors on one stripe count as a burst. */
const STRIPE_SOFT_ERROR_WINDOW_MS = 10000;
/**
 * Routes a stripe decoder error. Safari's main-thread VideoDecoder rejects
 * streams its worker decoder plays fine, so while the worker path is elected
 * for a full-frame encoder and still healthy the error is handoff noise: the
 * stripe decoder is rebuilt on the next keyframe instead of escalating into
 * the fallback ladder, which closes the socket and reloads. A burst that keeps
 * repeating within the window still reaches the ladder.
 * @param {*} e The decoder error.
 * @param {number} vncStripeYStart The stripe's row offset, which keys its decoder.
 */
function handleStripeDecodeError(e, vncStripeYStart) {
    if (decodeInWorker && !workerDecodeFailed && currentEncoderMode === 'h264enc') {
        const now = performance.now();
        const prev = stripeDecodeSoftErrors[vncStripeYStart];
        const soft = (prev && now - prev.last <= STRIPE_SOFT_ERROR_WINDOW_MS) ? prev.count + 1 : 1;
        stripeDecodeSoftErrors[vncStripeYStart] = { count: soft, last: now };
        if (soft <= 12) {
            console.warn(`stripe decoder error on Y=${vncStripeYStart} (worker path healthy; soft ${soft}/12):`, e && e.name);
            const info = vncStripeDecoders[vncStripeYStart];
            if (info) {
                try { info.decoder.close(); } catch (_) {}
                delete vncStripeDecoders[vncStripeYStart];
            }
            requestKeyframe();
            return;
        }
    }
    initiateFallback(e, `stripe_decoder_Y=${vncStripeYStart}`);
}

/**
 * Whether every stripe chunk handed to a stripe decoder has come back out.
 * @returns {boolean}
 */
function stripeDecodesDrained() {
  for (const key in vncStripeDecoders) {
    const info = vncStripeDecoders[key];
    if (!info || !info.decoder) continue;
    if (info.decoder.decodeQueueSize > 0) return false;
    if (info.pendingChunks && info.pendingChunks.length > 0) return false;
  }
  return true;
}

/**
 * Decodes the chunks a stripe queued while its decoder was still configuring.
 * @param {number} stripe_y_start
 */
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
/**
 * Main-thread back-buffer of the striped paths (h264enc-striped, jpeg).
 * Stripes accumulate here so damage-gated undamaged rows persist, and the
 * whole frame is blitted to the visible canvas once its last row has landed
 * or the stripe clock says it is complete — or, while stripes still flow, at
 * the frame-id boundary that proves it — so the display never shows a seam
 * between frame ids. Full-frame
 * h264enc presents one decoded frame atomically through the video sinks
 * instead.
 */
let stripeBackCanvas = null;
let stripeBackCtx = null;
let stripePendingFrameId = null;
let stripePendingDirty = false;
/** Newest frame id the striped composite has put on screen. */
let lastPresentedVideoFrameId = null;
/** When that id was presented, for the same reason as `lastReceivedVideoFrameAt`. */
let lastPresentedVideoFrameAt = 0;
/** When the striped composite holds a whole frame; see lib/stripe-clock.js. */
const stripeClock = createStripeClock();
/**
 * Creates the back-buffer, resized to the canvas.
 * @returns {CanvasRenderingContext2D|null}
 */
function ensureStripeBackBuffer() {
  if (!canvas) return null;
  if (!stripeBackCanvas) {
    stripeBackCanvas = document.createElement('canvas');
    stripeBackCtx = stripeBackCanvas.getContext('2d', { desynchronized: true });
  }
  if (stripeBackCanvas.width !== canvas.width || stripeBackCanvas.height !== canvas.height) {
    stripeBackCanvas.width = canvas.width;
    stripeBackCanvas.height = canvas.height;
    stripePendingFrameId = null;
    stripePendingDirty = false;
  }
  return stripeBackCtx;
}

/**
 * Source of the stripe compositor worker. It draws each decoded stripe onto an
 * OffscreenCanvas back-buffer and hands the finished frame back as one
 * ImageBitmap to blit, so the per-stripe compositing leaves the main thread
 * while the page keeps the decode and the reorder, damage and boundary logic.
 * The main-thread back-buffer is the fallback when a worker, OffscreenCanvas
 * or createImageBitmap is unavailable, or `offscreen_worker=false`.
 */
const STRIPE_WORKER_SRC = `
let back = null, bctx = null;
function ensureBack(w, h) {
  if (!back || back.width !== w || back.height !== h) {
    back = new OffscreenCanvas(w, h);
    bctx = back.getContext('2d', { desynchronized: true, alpha: false });
  }
}
self.onmessage = (e) => {
  const m = e.data;
  if (m.type === 'resize') { ensureBack(m.width, m.height); return; }
  if (m.type === 'stripe') {
    if (bctx) { try { bctx.drawImage(m.frame, 0, m.yPos); } catch (err) {} }
    try { m.frame.close(); } catch (err) {}
    return;
  }
  if (m.type === 'commit') {
    if (!back) return;
    createImageBitmap(back).then((bitmap) => { self.postMessage({ type: 'frame', bitmap: bitmap }, [bitmap]); }).catch(() => {});
    return;
  }
};
`;
let stripeWorker = null, stripeWorkerActive = false, stripeWorkerW = 0, stripeWorkerH = 0;

/** Terminates the stripe compositor worker. */
function deactivateStripeWorker() {
  if (stripeWorker) { try { stripeWorker.terminate(); } catch (e) { /* ignore */ } stripeWorker = null; }
  stripeWorkerActive = false; stripeWorkerW = 0; stripeWorkerH = 0;
}

/**
 * Creates the stripe compositor worker; idempotent.
 * @returns {boolean} False when it cannot run, so the caller composites on
 *     the main-thread back-buffer instead.
 */
function ensureStripeWorker() {
  if (stripeWorker) return true;
  if (!stripeCompositeEnabled || typeof Worker === 'undefined' || typeof OffscreenCanvas === 'undefined'
      || typeof createImageBitmap !== 'function') {
    return false;
  }
  try {
    const url = URL.createObjectURL(new Blob([STRIPE_WORKER_SRC], { type: 'text/javascript' }));
    stripeWorker = new Worker(url);
    URL.revokeObjectURL(url);
  } catch (e) {
    stripeWorker = null;
    return false;
  }
  console.info('[Selkies] striped codecs: compositing on an OffscreenCanvas in a worker.');
  stripeWorker.onerror = () => deactivateStripeWorker();
  stripeWorker.onmessage = (ev) => {
    const m = ev.data;
    if (!m || m.type !== 'frame') return;
    if (canvasContext && canvas && canvas.width > 0 && canvas.height > 0) {
      try { canvasContext.drawImage(m.bitmap, 0, 0); } catch (err) { /* ignore */ }
    }
    try { m.bitmap.close(); } catch (err) { /* ignore */ }
  };
  stripeWorkerW = 0; stripeWorkerH = 0;
  return true;
}

/**
 * Starts a stripe compositing cycle on the worker (its back-buffer resized to
 * the canvas) or the main-thread back-buffer.
 * @returns {boolean} False while the canvas has no size yet.
 */
function stripeCompositeBegin() {
  if (ensureStripeWorker()) {
    stripeWorkerActive = true;
    if (canvas.width > 0 && canvas.height > 0 && (stripeWorkerW !== canvas.width || stripeWorkerH !== canvas.height)) {
      stripeWorker.postMessage({ type: 'resize', width: canvas.width, height: canvas.height });
      stripeWorkerW = canvas.width; stripeWorkerH = canvas.height;
      stripePendingFrameId = null; stripePendingDirty = false;
    }
  } else {
    stripeWorkerActive = false;
    ensureStripeBackBuffer();
  }
  return !!(canvas && canvas.width > 0 && canvas.height > 0);
}

/**
 * Composites one decoded stripe at its row offset; always consumes the stripe.
 * @param {VideoFrame|ImageBitmap} stripe
 * @param {number} yPos
 */
function stripeCompositeDraw(stripe, yPos) {
  if (stripeWorkerActive && stripeWorker) {
    try { stripeWorker.postMessage({ type: 'stripe', frame: stripe, yPos: yPos }, [stripe]); return; }
    catch (e) { /* transfer failed; close below */ }
  } else if (stripeBackCtx) {
    try { stripeBackCtx.drawImage(stripe, 0, yPos); } catch (e) { /* ignore */ }
  }
  try { stripe.close(); } catch (e) { /* ignore */ }
}

/**
 * Presents the composited frame: the worker commits an ImageBitmap, the main
 * thread blits its back-buffer. Counted as the striped modes' displayed frame,
 * which is what `window.fps` reports for them.
 */
function stripeCompositePresent() {
  frameCount++;
  lastPresentedVideoFrameId = stripePendingFrameId;
  lastPresentedVideoFrameAt = performance.now();
  if (stripeWorkerActive && stripeWorker) {
    try { stripeWorker.postMessage({ type: 'commit' }); } catch (e) { /* ignore */ }
  } else if (canvasContext && canvas.width > 0 && canvas.height > 0) {
    canvasContext.drawImage(stripeBackCanvas, 0, 0);
  }
}
/** Newest JPEG-stripe frame id drawn per row offset, so older out-of-order stripes are skipped. */
let lastDrawnJpegStripeFrameId = {};

/** Disarms the START_VIDEO watchdog. */
function clearStartVideoWatchdog() {
  if (startVideoWatchdogTimer !== null) {
    clearTimeout(startVideoWatchdogTimer);
    startVideoWatchdogTimer = null;
  }
  startVideoWatchdogAttempts = 0;
}

/**
 * Proves a stream the returning tab believes is running: a reconnect or
 * reload while hidden can leave the server holding this display stopped, and
 * a screen with no damage since sends nothing to repaint the cleared canvas
 * either way. A keyframe request answers the second case; if nothing arrives
 * within `VISIBLE_FRAME_PROBE_MS`, the stream really is stopped and is restarted.
 */
function armVisibleFrameProbe() {
  if (isSharedMode) return;
  const chunksBefore = window.videoChunksReceived;
  requestKeyframe();
  setTimeout(() => {
    if (document.hidden || window.videoChunksReceived !== chunksBefore) return;
    if (!isVideoPipelineActive) return;
    if (!websocket || websocket.readyState !== WebSocket.OPEN) return;
    console.warn('No video since the tab came back; restarting the stream.');
    try { websocket.send('START_VIDEO'); } catch (_) { return; }
    armStartVideoWatchdog();
  }, VISIBLE_FRAME_PROBE_MS);
}

/**
 * Resends START_VIDEO while no video arrives, up to the attempt limit, then
 * forces a reconnect through the onclose path. Stands down when the tab is
 * hidden again (the visibilitychange path owns that state, and a shared
 * viewer's resume can be rate-limited by the server) or the socket is not open
 * (the reconnect logic owns recovery).
 */
function onStartVideoWatchdogTimeout() {
  startVideoWatchdogTimer = null;
  if (document.hidden) { startVideoWatchdogAttempts = 0; return; }
  if (!websocket || websocket.readyState !== WebSocket.OPEN) { startVideoWatchdogAttempts = 0; return; }
  startVideoWatchdogAttempts++;
  if (startVideoWatchdogAttempts <= START_VIDEO_WATCHDOG_MAX_ATTEMPTS) {
    console.warn(`No video after START_VIDEO; resend attempt ${startVideoWatchdogAttempts}/${START_VIDEO_WATCHDOG_MAX_ATTEMPTS}.`);
    try { websocket.send('START_VIDEO'); } catch (_) {}
    startVideoWatchdogTimer = setTimeout(onStartVideoWatchdogTimeout, START_VIDEO_WATCHDOG_MS);
  } else {
    console.warn('START_VIDEO watchdog exhausted; forcing websocket reconnect.');
    startVideoWatchdogAttempts = 0;
    try { websocket.close(); } catch (_) {}
  }
}

/** Arms the START_VIDEO watchdog with a fresh attempt count for this visibility cycle. */
function armStartVideoWatchdog() {
  if (startVideoWatchdogTimer !== null) clearTimeout(startVideoWatchdogTimer);
  startVideoWatchdogAttempts = 0;
  startVideoWatchdogTimer = setTimeout(onStartVideoWatchdogTimeout, START_VIDEO_WATCHDOG_MS);
}

/** Disarms the shared-mode stall watchdog. */
function clearSharedStallWatchdog() {
  if (sharedStallWatchdogId !== null) {
    clearInterval(sharedStallWatchdogId);
    sharedStallWatchdogId = null;
  }
  sharedStallRecoveryAttempts = 0;
  sharedStallNextRecoveryTime = 0;
}

/**
 * Arms the shared-mode stall watchdog (see `sharedStallWatchdogId`). While the
 * viewer is hidden, paused or not yet ready it expects no chunks, so the clock
 * is kept fresh and the watchdog cannot fire the instant those states end.
 */
function armSharedStallWatchdog() {
  if (!isSharedMode || sharedStallWatchdogId !== null) return;
  lastSharedVideoChunkTime = performance.now();
  sharedStallRecoveryAttempts = 0;
  sharedStallNextRecoveryTime = 0;
  sharedStallWatchdogId = setInterval(() => {
    if (document.hidden || sharedVideoPaused || sharedClientState !== 'ready') {
      lastSharedVideoChunkTime = performance.now();
      return;
    }
    if (!websocket || websocket.readyState !== WebSocket.OPEN) return;
    const now = performance.now();
    const silence = now - lastSharedVideoChunkTime;
    if (silence < SHARED_STALL_TIMEOUT_MS) return;
    if (now < sharedStallNextRecoveryTime) return;
    sharedStallRecoveryAttempts++;
    const backoff = Math.min(
      SHARED_STALL_TIMEOUT_MS * Math.pow(2, sharedStallRecoveryAttempts - 1),
      SHARED_STALL_MAX_BACKOFF_MS);
    sharedStallNextRecoveryTime = now + backoff;
    console.warn(`Shared mode: no video chunk for ${Math.round(silence)}ms; ` +
      `resending START_VIDEO (attempt ${sharedStallRecoveryAttempts}, next retry in ${backoff}ms).`);
    try { websocket.send('START_VIDEO'); } catch (_) { /* onclose path recovers */ }
  }, 1000);
}

/**
 * Output callback of the stripe decoders. A full-frame h264enc frame (the
 * single decoder at row 0) is presented the instant it decodes, for the lowest
 * glass-to-glass latency, superseding anything still queued: through the
 * main-thread track generator, else the worker sink, else the canvas.
 * h264enc-striped composites partial-height stripes and drains through the
 * rAF queue instead.
 * @param {number} yPos The stripe's row offset.
 * @param {VideoFrame} frame
 */
function handleDecodedVncStripeFrame(yPos, frame) {
  if (currentEncoderMode === 'h264enc' && yPos === 0) {
    if (document.hidden || (clientMode === 'websockets' && !isSharedMode && !isVideoPipelineActive)) {
      try { frame.close(); } catch (e) {}
      return;
    }
    if (decodedStripesQueue.length > 0) {
      for (const stale of decodedStripesQueue) { try { stale.frame.close(); } catch (e) {} }
      decodedStripesQueue.length = 0;
    }
    if (supportsWindowMSTG && presentFrameToVideo(frame)) {
      // Handed to the main-thread track generator.
    } else if (USE_OFFSCREEN_WORKER && presentFrameToWorker(frame)) {
      // Handed to the worker sink.
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
    frame,
    frameId: frame.timestamp
  });
}

/** HTTP uploads and the drag-drop/file-picker plumbing (lib/file-upload.js); shared viewers never upload. */
const fileUploader = createFileUploader({ canUpload: () => !isSharedMode });
const handleRequestFileUpload = fileUploader.handleRequestFileUpload;
const handleFileInputChange = fileUploader.handleFileInputChange;
const handleDragOver = fileUploader.handleDragOver;
const handleDrop = fileUploader.handleDrop;

/** Requests a screen wake lock so the device does not sleep mid-session. */
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
      console.warn(`Could not acquire Wake Lock: ${err.name}, ${err.message}`);
    }
  } else {
    console.warn('Wake Lock API is not supported by this browser.');
  }
};

/** Releases the screen wake lock if one is held. */
const releaseWakeLock = async () => {
  if (wakeLockSentinel !== null) {
    await wakeLockSentinel.release();
    wakeLockSentinel = null;
  }
};

/**
 * Trailing-edge debounce.
 * @param {Function} func
 * @param {number} delay Milliseconds of quiet before `func` runs.
 * @returns {Function}
 */
function debounce(func, delay) {
  let timeoutId;
  return function(...args) {
    clearTimeout(timeoutId);
    timeoutId = setTimeout(() => {
      func.apply(this, args);
    }, delay);
  };
}

/** Marks the stream as started and hides the status bar and start button. */
const startStream = () => {
  if (streamStarted) return;
  streamStarted = true;
  if (statusDisplayElement) statusDisplayElement.classList.add('hidden');
  if (playButtonElement) playButtonElement.classList.add('hidden');
  console.log("Stream started (UI elements hidden).");
};

/**
 * Creates the Input handler on the overlay input once the server has assigned
 * the client's role and slot, wires its dashboard chords to the dashboards
 * (`toggleDashboard`, `toggleTouchGamepad` window messages; fullscreen,
 * Ctrl+Shift+F, stays inside Input), publishes it as
 * `window.webrtcInput`, installs the automatic or manual resize handling, and
 * attaches file drop and mobile keyboard assistance. A viewer role keeps the
 * gamepad but has its pointer and keyboard context detached.
 */
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

  inputInstance.onmenuhotkey = () => {
    window.postMessage({ type: 'toggleDashboard' }, window.location.origin);
  };
  inputInstance.ongamepadhotkey = () => {
    window.postMessage({ type: 'toggleTouchGamepad' }, window.location.origin);
  };
  inputInstance.gamingMode = gamingModeActive;
  inputInstance.ongamingmode = (active) => {
    gamingModeActive = active;
    window.postMessage({ type: 'gamingModeUpdate', active }, window.location.origin);
  };

  inputInstance.getWindowResolution = () => {
    const videoContainer = document.querySelector('.video-container');
    if (!videoContainer) {
      console.warn('initializeInput: .video-container not found, using window inner dimensions for resolution calculation.');
      return [window.innerWidth, window.innerHeight];
    }
    const videoContainerRect = videoContainer.getBoundingClientRect();
    return [videoContainerRect.width, videoContainerRect.height];
  };

  // Runs for a pad already present during attach(), before window.webrtcInput
  // is assigned, so the manager is read off the instance.
  inputInstance.ongamepadconnected = (gamepad_id) => {
    gamepad.gamepadState = 'connected';
    gamepad.gamepadName = gamepad_id;
    console.log(`Client: Gamepad "${gamepad_id}" connected. isSharedMode: ${isSharedMode}, isGamepadEnabled (global toggle): ${isGamepadEnabled}`);
    const manager = inputInstance.gamepadManager;
    if (manager) {
        if (isSharedMode) {
            manager.enable();
            console.log("Shared mode: Gamepad connected, ensuring its GamepadManager is active for polling.");
        } else {
            if (!isGamepadEnabled) {
                manager.disable();
                console.log("Primary mode: Gamepad connected, but master gamepad toggle is OFF. Disabling its GamepadManager.");
            } else {
                manager.enable();
                console.log("Primary mode: Gamepad connected, master gamepad toggle is ON. Ensuring its GamepadManager is active.");
            }
        }
    } else {
        console.warn("Client: gamepadManager not found in ongamepadconnected. Cannot control its polling state.");
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
  inputInstance.setDisplayLayouts(latestDisplayLayouts, displayId);
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

  /**
   * Automatic resize: sends the aligned, capped viewport size and restyles
   * the canvas. Skipped in shared and manual mode, and on the primary when
   * `enable_resize=false` pins its resolution server-side (a secondary's
   * resize is its layout bring-up and stays allowed, matching the server).
   * Stripe decoders are closed first, since rows that vanish on shrink would
   * keep a live decoder nothing feeds, and the divergence flag is reset for
   * stream_resolution to re-flag.
   */
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
    if (window.manual_resolution) {
      console.log("handleResizeUI: Auto-resize skipped, manual resolution mode is active.");
      return;
    }
    if (window.enable_resize === false && displayId !== 'display2') {
      console.log("handleResizeUI: Auto-resize skipped, dynamic resizing is disabled.");
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

    clearAllVncStripeDecoders();
    window.streamResolutionDiverged = false;
    sendResolutionToServer(evenWidth, evenHeight);
    resetCanvasStyle(evenWidth, evenHeight);
  };

  handleResizeUI_globalRef = handleResizeUI;
  originalWindowResizeHandler = debounce(() => { maybeFollowDpr(); handleResizeUI(); }, 500);

  /**
   * Re-runs the automatic resize when devicePixelRatio changes. The stream
   * resolution is logical size times DPR, but a DPR change alone (a window
   * dragged to a monitor of another density, an OS scaling change) fires no
   * resize event. While `scaling_dpi` sits on its automatic default it is
   * re-derived too, so the remote UI density follows the display the window
   * is on. matchMedia resolution queries are one-shot at a given dppx, so
   * the query is re-armed after each change.
   */
  const watchDevicePixelRatio = () => {
    let mql = null;
    const onDprChange = () => {
      maybeFollowDpr();
      if (typeof handleResizeUI_globalRef === 'function') handleResizeUI_globalRef();
      arm();
    };
    const arm = () => {
      if (mql) { try { mql.removeEventListener('change', onDprChange); } catch (_) {} }
      const dpr = window.devicePixelRatio || 1;
      mql = window.matchMedia(`(resolution: ${dpr}dppx)`);
      mql.addEventListener('change', onDprChange, { once: true });
    };
    arm();
    // An emulated density change fires neither the query nor a resize;
    // a slow poll of the live value catches those too.
    setInterval(maybeFollowDpr, 1000);
  };
  watchDevicePixelRatio();

  if (isSharedMode) {
    console.log("Shared mode: Auto-resize event listener (originalWindowResizeHandler) NOT attached.");
  } else if (!window.manual_resolution) {
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
    // Typed characters go through Input's own listener on this element; only
    // the control keys mobile keyboards emit as keydown are forwarded here.
    keyboardInputAssist.addEventListener('keydown', (event) => {
      if (event.key === 'Enter' || event.keyCode === 13) {
        inputInstance._sendMomentaryKey(0xFF0D);
        event.preventDefault();
        keyboardInputAssist.value = '';
      } else if (event.key === 'Backspace' || event.keyCode === 8) {
        inputInstance._sendMomentaryKey(0xFF08);
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

/**
 * Routes playback to the preferred output device. Audio plays out of the
 * AudioContext (no media element carries it), so this needs
 * `AudioContext.setSinkId`; where it is missing, or the context is not
 * running yet, playback stays on the default device.
 */
async function applyOutputDevice() {
  if (!preferredOutputDeviceId) {
    console.log("No preferred output device set, using default.");
    return;
  }
  const supportsSinkId = typeof AudioContext !== 'undefined' && 'setSinkId' in AudioContext.prototype;
  if (!supportsSinkId) {
    console.warn("Browser does not support setSinkId, cannot apply output device preference.");
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

/** Posts `sidebarButtonStatusUpdate` with the state of every pipeline toggle to the dashboards. */
function postSidebarButtonUpdate() {
  const updatePayload = {
    type: 'sidebarButtonStatusUpdate',
    video: isVideoPipelineActive,
    audio: isAudioPipelineActive,
    microphone: isMicrophoneActive,
    webcam: isWebcamActive,
    gamepad: isGamepadEnabled
  };
  console.log('Posting sidebarButtonStatusUpdate:', updatePayload);
  window.postMessage(updatePayload, window.location.origin);
}

/**
 * Handles the window messages the dashboards post to the core (same origin
 * only): volume and mute, local scaling, the virtual keyboard, CSS scaling,
 * anti-aliasing, cursor rendering, manual resolution and its reset, pipeline
 * and gamepad control, audio device selection, stream commands, clipboard
 * pushes, and the `getStats` and `settings` requests. See the module
 * docblock for the full vocabulary. A `setUseCssScaling` with `persist:
 * false` is server-authored and leaves the user's stored key untouched; the
 * resolution paths honour `enable_resize=false`, which pins the primary's
 * resolution server-side while a secondary stays resizable; and
 * `clipboardImageUpdate` reports every skip so a dead click never reads as a
 * bug.
 * @param {MessageEvent} event
 */
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
      // Accepted for frontends to signal; the core has nothing to throttle.
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
        if (window.manual_resolution && manual_width !== null && manual_height !== null) {
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
        if (message.persist !== false) {
          setBoolParam('useCssScaling', useCssScaling);
        }
        console.log(`Set useCssScaling to ${useCssScaling}${message.persist === false ? '.' : ' and persisted.'}`);

        if (window.webrtcInput && typeof window.webrtcInput.updateCssScaling === 'function') {
          window.webrtcInput.updateCssScaling(useCssScaling);
        }
        if (changed) {
          updateCanvasImageRendering();
          if (window.manual_resolution && manual_width != null && manual_height != null) {
            sendResolutionToServer(manual_width, manual_height);
            applyManualCanvasStyle(manual_width, manual_height, scaleLocallyManual);
          } else if (!isSharedMode) {
            if (window.enable_resize !== false || displayId === 'display2') {
              const currentWindowRes = window.webrtcInput ? window.webrtcInput.getWindowResolution() : [window.innerWidth, window.innerHeight];
              const autoWidth = alignResolution(currentWindowRes[0]);
              const autoHeight = alignResolution(currentWindowRes[1]);
              sendResolutionToServer(autoWidth, autoHeight);
              resetCanvasStyle(autoWidth, autoHeight);
            }
          } else {
             if (manual_width && manual_height) {
                applyManualCanvasStyle(manual_width, manual_height, true);
             }
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
      window.manual_resolution = true;
      manual_width = alignResolution(width);
      manual_height = alignResolution(height);
      console.log(`Rounded logical resolution to even numbers: ${manual_width}x${manual_height}`);
      setIntParam('manual_width', manual_width);
      setIntParam('manual_height', manual_height);
      setBoolParam('manual_resolution', true);
      disableAutoResize();
      sendResolutionToServer(manual_width, manual_height);
      applyManualCanvasStyle(manual_width, manual_height, scaleLocallyManual);
      if (currentEncoderMode === 'h264enc' || currentEncoderMode === 'h264enc-striped') {
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
      window.manual_resolution = false;
      manual_width = null;
      manual_height = null;
      setIntParam('manual_width', null);
      setIntParam('manual_height', null);
      setBoolParam('manual_resolution', false);
      if (window.enable_resize !== false || displayId === 'display2') {
        const currentWindowRes = window.webrtcInput ? window.webrtcInput.getWindowResolution() : [window.innerWidth, window.innerHeight];
        const autoWidth = alignResolution(currentWindowRes[0]);
        const autoHeight = alignResolution(currentWindowRes[1]);
        resetCanvasStyle(autoWidth, autoHeight);
        if (currentEncoderMode === 'h264enc' || currentEncoderMode === 'h264enc-striped') {
          console.log("Clearing VNC stripe decoders due to resolution reset to window.");
          clearAllVncStripeDecoders();
          if (canvasContext) canvasContext.setTransform(1, 0, 0, 1, 0, 0);
          canvasContext.clearRect(0, 0, canvas.width, canvas.height);
        }
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
      sendExplicitClipboard(message.text);
      break;
    case 'clipboardImageUpdate': {
      if (isSharedMode) {
        console.log("Shared mode: Clipboard image write to server blocked.");
        notifyClipboardImageSkip('viewers cannot set the clipboard', 'clipboardSkipReadonly');
        break;
      }
      if (!message.imageBlob) {
        notifyClipboardImageSkip('no image selected', 'clipboardSkipNoImage');
        break;
      }
      if (!enable_binary_clipboard) {
        notifyClipboardImageSkip('image clipboard is disabled on the server (enable_binary_clipboard)', 'clipboardSkipBinaryDisabled');
        break;
      }
      // Recorded here, not after the blob is read: the file picker's own
      // refocus fires a clipboard read that must already see this push.
      sendExplicitClipboard(
        message.imageBlob, message.imageBlob.type || 'image/png', notifyClipboardImageSkip
      ).catch((e) => {
        console.warn('Failed to send uploaded clipboard image:', e);
        notifyClipboardImageSkip('send failed: ' + e.message, 'clipboardSkipSendFailed');
      });
      break;
    }
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
          if (websocket && websocket.setAudioActive) websocket.setAudioActive(isAudioPipelineActive);
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
      } else if (pipeline === 'webcam') {
        if (isSharedMode) {
          console.log("Shared mode: Webcam control blocked.");
          break;
        }
        if (!webcamEnabled) {
          console.log("Webcam is disabled. Webcam pipeline control blocked.");
          break;
        }
        if (desiredState) {
          startWebcamCapture();
        } else {
          stopWebcamCapture();
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
      enterFullscreen(false);
      break;
    case 'requestGamingMode':
      enterFullscreen(true);
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

/**
 * Tells the dashboard why a clipboard-image upload was skipped, in the
 * `fileUpload` warning channel transfer warnings already use.
 * @param {string} reason Human-readable reason.
 * @param {string} code Translation key the dashboards map to a localized message.
 */
function notifyClipboardImageSkip(reason, code) {
  console.warn('Clipboard image upload skipped: ' + reason);
  window.postMessage({
    type: 'fileUpload',
    payload: { status: 'warning', fileName: 'clipboard-image', message: reason, code },
  }, window.location.origin);
}

/**
 * Tells the dashboard that a server image never reached the local clipboard.
 *
 * The panel shows nothing of an inbound image but this notice, so a write the
 * browser refuses would otherwise read as the feature not working at all.
 * @param {*} error What the write threw.
 */
function notifyClipboardImageWriteFailed(error) {
  const reason = localClipboardBlocker() || (error && error.message) || String(error);
  console.error('Failed to write the session image to the local clipboard:', error);
  window.postMessage({
    type: 'fileUpload',
    payload: {
      status: 'warning', fileName: 'clipboard-image', message: reason,
      code: 'clipboardImageWriteFailed',
    },
  }, window.location.origin);
}

/**
 * Sends local clipboard content to the server as a chunked transfer
 * (lib/clipboard-worker-bridge.js, the same wire protocol and worker offload
 * as the WebRTC core), gated on the clipboard-in setting and the change-only
 * sync. A bufferedAmount backpressure gate keeps a burst from starving the
 * microphone and input on the same socket; only a completed transfer marks
 * the content synced, so an aborted one stays re-sendable.
 * @param {string|ArrayBuffer|Uint8Array} data Text, or image bytes.
 * @param {string} [mimeType] Forced to `text/plain` for text.
 * @param {Function|null} [onSkip] Called with reason and code when nothing was sent.
 */
async function sendClipboardData(data, mimeType = 'text/plain', onSkip = null) {
    const skip = (reason, code) => { if (onSkip) onSkip(reason, code); };
    if (window.clipboard_enabled === undefined) {
        skip('the session has not reported its clipboard policy yet', 'clipboardSkipNotConnected');
        return;
    }
    if (!window.clipboard_enabled) {
        skip('the server has the clipboard turned off', 'clipboardSkipDisabled');
        return;
    }
    if (!clipboard_in_enabled) {
        skip('the client-to-session clipboard is turned off', 'clipboardSkipInDisabled');
        return;
    }
    if (!websocket || websocket.readyState !== WebSocket.OPEN) {
        console.warn('Cannot send clipboard data: WebSocket is not open.');
        skip('not connected', 'clipboardSkipNotConnected');
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
    // Binary content is compared by the digest the worker takes, so the page
    // does not walk a payload of any size to decide whether to send it.
    let subject = data;
    if (isBinary) {
        try {
            const { byteLength, hash } = await clipboardWorker.hashBytes(dataBytes.slice().buffer);
            subject = digestedPayload(byteLength, hash);
        } catch (_) { /* the worker is gone; the page's own hash still decides */ }
    }
    if (!clipboardSync.shouldSend(subject, mimeType)) {
        skip('already the current clipboard', 'clipboardSkipUnchanged');
        return;
    }
    let transferAborted = false;
    await sendClipboardChunked(dataBytes, mimeType, {
        worker: clipboardWorker,
        send: (m) => websocket.send(m),
        waitDrain: async () => {
            while (websocket.bufferedAmount > CLIPBOARD_BACKLOG_BYTES) {
                await new Promise(resolve => setTimeout(resolve, CLIPBOARD_BACKLOG_POLL_MS));
                if (websocket.readyState !== WebSocket.OPEN) {
                    transferAborted = true;
                    return false;
                }
            }
            return true;
        },
        chunkRawBytes: CLIPBOARD_CHUNK_SIZE,
        nextTid: () => ++clipboardTransferCounter,
    });
    if (!transferAborted && websocket.readyState === WebSocket.OPEN) {
        clipboardSync.markSynced(subject, mimeType);
    } else {
        skip('connection lost during send', 'clipboardSkipSendFailed');
    }
}

/**
 * Applies a settings payload to the runtime and pushes the result to the
 * server. A dashboard-authored payload is persisted; a server-authored one
 * (the locked and overridden values replayed on every connect) is applied but
 * never written to the user's own keys, where it would outlive the lock and
 * masquerade as their pick. An encoder switch tears the decoders down and
 * asks for a keyframe once the server's restart settles, in case its restart
 * IDR beat the reset over the wire.
 * @param {Object<string, *>} settings Keys named as the server knows them.
 * @param {boolean} [fromServer]
 */
function handleSettingsMessage(settings, fromServer) {
  const storeInt = fromServer ? () => {} : setIntParam;
  const storeBool = fromServer ? () => {} : setBoolParam;
  const storeString = fromServer ? () => {} : setStringParam;
  console.log('Applying settings:', settings);
  let settingsChanged = false;
  if (settings.framerate !== undefined) {
    framerate = parseInt(settings.framerate);
    storeInt('framerate', framerate);
    settingsChanged = true;
  }
  if (settings.webcam_encoder !== undefined) {
    const preference = String(settings.webcam_encoder);
    if (WEBCAM_ENCODER_PREFERENCES.includes(preference) && preference !== webcamEncoderPreference) {
      webcamEncoderPreference = preference;
      storeString('webcam_encoder', preference);
      if (webcamCapture) {
        stopWebcamCapture();
        startWebcamCapture();
      }
    }
  }
  if (settings.encoder !== undefined) {
    let newEncoderSetting = settings.encoder;
    if (!canDecodeEncoder(newEncoderSetting)) {
      if (fromServer) {
        showUndecodableEncoderNotice(newEncoderSetting);
      } else {
        console.warn(`Encoder ${newEncoderSetting} needs WebCodecs, which this browser lacks; keeping jpeg.`);
      }
      newEncoderSetting = 'jpeg';
    } else {
      clearUndecodableEncoderNotice();
    }
    if (currentEncoderMode !== newEncoderSetting) {
        currentEncoderMode = newEncoderSetting;
        updateVideoDivert();
        storeString('encoder', currentEncoderMode);
        settingsChanged = true;
        if (newEncoderSetting === 'jpeg' || newEncoderSetting === 'h264enc' || newEncoderSetting === 'h264enc-striped') {
        }
        if (newEncoderSetting !== 'h264enc-striped') {
            clearAllVncStripeDecoders();
        }
        cleanupJpegStripeQueue();
        clearDecodedStripesQueue();
        setTimeout(() => {
            if (websocket && websocket.readyState === WebSocket.OPEN) {
                try { websocket.send('REQUEST_KEYFRAME'); } catch (e) { /* reconnect path covers it */ }
            }
        }, 1500);
    }
  }
  if (settings.video_crf !== undefined) {
    video_crf = parseInt(settings.video_crf, 10);
    storeInt('video_crf', video_crf);
    settingsChanged = true;
  }
  if (settings.video_fullcolor !== undefined) {
    video_fullcolor = !!settings.video_fullcolor;
    storeBool('video_fullcolor', video_fullcolor);
    settingsChanged = true;
    clearAllVncStripeDecoders();
  }
  if (settings.video_streaming_mode !== undefined) {
    video_streaming_mode = !!settings.video_streaming_mode;
    storeBool('video_streaming_mode', video_streaming_mode);
    settingsChanged = true;
  }
  if (settings.jpeg_quality !== undefined) {
    jpeg_quality = parseInt(settings.jpeg_quality, 10);
    storeInt('jpeg_quality', jpeg_quality);
    settingsChanged = true;
  }
  if (settings.paint_over_jpeg_quality !== undefined) {
    paint_over_jpeg_quality = parseInt(settings.paint_over_jpeg_quality, 10);
    storeInt('paint_over_jpeg_quality', paint_over_jpeg_quality);
    settingsChanged = true;
  }
  if (settings.use_cpu !== undefined) {
    use_cpu = !!settings.use_cpu;
    storeBool('use_cpu', use_cpu);
    settingsChanged = true;
    clearAllVncStripeDecoders();
  }
  if (settings.video_paintover_crf !== undefined) {
    video_paintover_crf = parseInt(settings.video_paintover_crf, 10);
    storeInt('video_paintover_crf', video_paintover_crf);
    settingsChanged = true;
  }
  if (settings.video_paintover_burst_frames !== undefined) {
    video_paintover_burst_frames = parseInt(settings.video_paintover_burst_frames, 10);
    storeInt('video_paintover_burst_frames', video_paintover_burst_frames);
    settingsChanged = true;
  }
  if (settings.use_paint_over_quality !== undefined) {
    use_paint_over_quality = !!settings.use_paint_over_quality;
    storeBool('use_paint_over_quality', use_paint_over_quality);
    settingsChanged = true;
  }
  if (settings.scaling_dpi !== undefined) {
    scalingDPI = parseInt(settings.scaling_dpi, 10);
    // Not stored: the localStorage pin is the dashboard's explicit slider pick;
    // the payload builder rides the live value either way.
    settingsChanged = true;
  }
  if (settings.enable_binary_clipboard !== undefined) {
    enable_binary_clipboard = !!settings.enable_binary_clipboard;
    storeBool('enable_binary_clipboard', enable_binary_clipboard);
    settingsChanged = true;
  }
  if (settings.clipboard_in_enabled !== undefined) {
    clipboard_in_enabled = !!settings.clipboard_in_enabled;
    storeBool('clipboard_in_enabled', clipboard_in_enabled);
    settingsChanged = true;
  }
  if (settings.clipboard_out_enabled !== undefined) {
    clipboard_out_enabled = !!settings.clipboard_out_enabled;
    storeBool('clipboard_out_enabled', clipboard_out_enabled);
    settingsChanged = true;
  }
  if (settings.use_css_scaling !== undefined) {
    const messageData = { type: 'setUseCssScaling', value: !!settings.use_css_scaling, persist: !fromServer };
    receiveMessage({ origin: window.location.origin, data: messageData });
  }
  if (settings.use_browser_cursors !== undefined) {
    // Only the setUseBrowserCursors message persists this value.
    use_browser_cursors = !!settings.use_browser_cursors;
    applyEffectiveCursorSetting();
  }
  if (settings.debug !== undefined) {
    debug = settings.debug;
    // Persisted even when server-authored: the reload settles on the stored flag.
    setBoolParam('debug', debug);
    console.log(`Applied debug setting: ${debug}. Reloading...`);
    setTimeout(() => { window.location.reload(); }, 700);
    return;
  }
  if (settings.rate_control_mode !== undefined) {
    rateControlMode = settings.rate_control_mode;
    storeString('rate_control_mode', rateControlMode);
    fetchLatestRCvalue(rateControlMode);
    settingsChanged = true;
  }
  if (settings.video_bitrate !== undefined) {
    videoBitrate = parseInt(settings.video_bitrate, 10);
    storeInt('video_bitrate', videoBitrate);
    settingsChanged = true;
  }
  if (settings.audio_bitrate !== undefined) {
    audio_bitrate = parseInt(settings.audio_bitrate, 10);
    storeInt('audio_bitrate', audio_bitrate);
    settingsChanged = true;
  }
  if (settings.force_aligned_resolution !== undefined) {
    force_aligned_resolution = !!settings.force_aligned_resolution;
    storeBool('force_aligned_resolution', force_aligned_resolution);
    settingsChanged = true;
  }
  if (settingsChanged) {
    sendFullSettingsUpdateToServer('handleSettingsMessage');
  }
}

/**
 * Re-reads the stored value the new rate-control mode governs (bitrate for
 * `cbr`, CRF for `crf`).
 * @param {string} newMode
 */
function fetchLatestRCvalue(newMode) {
  if (newMode === "cbr") {
    videoBitrate = getIntParam('video_bitrate', videoBitrate);
  } else if (newMode === "crf") {
    video_crf = getIntParam('video_crf', video_crf);
  }
};

/** Posts a `stats` snapshot (server, network, client fps, buffers, pipeline state) to the parent window. */
function sendStatsMessage() {
  const stats = {
    gpu: gpuStat,
    cpu: cpuStat,
    network: networkStat,
    clientFps: window.fps,
    audioBuffer: window.currentAudioBufferSize,
    audioUnderrunSamples: window.currentAudioUnderrunSamples,
    audioDropped: window.currentAudioDropped + window.currentAudioWorkletDropped,
    isVideoPipelineActive: isVideoPipelineActive,
    isAudioPipelineActive: isAudioPipelineActive,
    isMicrophoneActive: isMicrophoneActive,
    isWebcamActive: isWebcamActive,
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

/**
 * Runs the connection: pre-flight checks and the page build, the clipboard
 * gesture wiring, the tab visibility handling, the paint loop, audio setup,
 * and the socket with its message dispatch, reconnect and fallback paths.
 * Called once the document has loaded.
 */
function initWebsockets() {
  if (!runPreflightChecks()) {
    return;
  }

  const pathname = getRoutePrefix() + '/';

  /** Focus and gesture local-to-server clipboard sync (lib/clipboard-sync.js); text is deduped server-side. */
  localClipboardSender = createLocalClipboardSender({
    isChromium,
    getDeferredWriteInFlight: () => deferredClipboardWriter.getInFlight(),
    isSharedMode: () => isSharedMode,
    canSync: () => !!window.clipboard_enabled,
    canRead: () => !!clipboard_in_enabled,
    binaryEnabled: () => !!enable_binary_clipboard,
    sendClipboardData: (data, mime, onSkip) => sendClipboardData(data, mime, onSkip),
  });
  const readLocalClipboardAndSend = () => localClipboardSender.readAndSend();
  const maybeSendInitialClipboard = () => localClipboardSender.maybeInitial();

  // Firefox and WebKit raise a paste prompt on every focus read, so only
  // Chromium reads on focus; the others read on the paste gestures.
  if (isChromium) {
    window.addEventListener('focus', () => { readLocalClipboardAndSend(); });
  }

  /** Paste-ordering hold and the copy/paste gestures (lib/clipboard-sync.js); only the gates and the send are per-core. */
  const clipboardGestures = createClipboardGestures({
    isChromium,
    clipboardSync,
    sendClipboardData: (data, mime) => sendClipboardData(data, mime),
    canSync: () => !isSharedMode && !!window.clipboard_enabled,
    canRead: () => !!clipboard_in_enabled,
    canWrite: () => !!clipboard_out_enabled,
    binaryEnabled: () => !!enable_binary_clipboard,
    getSendInFlight: () => localClipboardSender.getSendInFlight(),
    getDeferredWriteInFlight: () => deferredClipboardWriter.getInFlight(),
  });
  clipboardGestures.wire();

  /** Clears the canvas so a paused stream does not show a stale frame. */
  const clearVideoCanvasVisually = () => {
    if (canvasContext && canvas) {
      try {
        canvasContext.setTransform(1, 0, 0, 1, 0, 0);
        canvasContext.clearRect(0, 0, canvas.width, canvas.height);
      } catch (e) { console.error("Error clearing canvas on visibility change:", e); }
    }
  };
  let hiddenVideoStopTimer = null;
  /** Whether the tab-hide handler paused the video; only its own pause is resumed, a dashboard stop stays stopped. */
  let videoPausedForHiddenTab = false;
  /**
   * Pauses video while the tab is hidden and resumes it on show. A shared
   * viewer pauses only its own feed (the server drops this socket from the
   * broadcast and resumes it with a reset and IDR; control, cursor and audio
   * stay live). A controller's pause is deferred, because a navigating
   * document reports hidden just before it unloads and a STOP_VIDEO sent then
   * races the successor connection; timers never fire in an unloading
   * document. The resume is keyed on the pause this handler performed, never
   * on the pipeline flag, which a status sync could flip while hidden. The
   * wake lock, dropped by the browser on hide, is re-taken after the resume
   * send, never in front of it. A decoder the browser reclaimed in the
   * background is re-created lazily by the frame sink.
   */
  document.addEventListener('visibilitychange', async () => {
    if (isSharedMode) {
      if (!websocket || websocket.readyState !== WebSocket.OPEN) return;
      if (document.hidden) {
        if (!sharedVideoPaused) {
          sharedVideoPaused = true;
          try { websocket.send('STOP_VIDEO'); } catch (_) {}
          clearVideoCanvasVisually();
          window.postMessage({ type: 'pipelineStatusUpdate', video: false }, window.location.origin);
          console.log("Shared mode: tab hidden, sent STOP_VIDEO to pause this viewer's feed.");
        }
      } else {
        if (sharedVideoPaused) {
          sharedVideoPaused = false;
          try { websocket.send('START_VIDEO'); } catch (_) {}
          armStartVideoWatchdog();
          window.postMessage({ type: 'pipelineStatusUpdate', video: true }, window.location.origin);
          console.log("Shared mode: tab visible, sent START_VIDEO to resume this viewer's feed.");
        }
        if (wakeLockSentinel === null) {
          console.log('Tab is visible again, re-acquiring Wake Lock.');
          await requestWakeLock();
        }
      }
      return;
    }
    if (document.hidden) {
      if (hiddenVideoStopTimer === null) {
        hiddenVideoStopTimer = setTimeout(() => {
          hiddenVideoStopTimer = null;
          if (!document.hidden) return;
          console.log('Tab is hidden, stopping video pipeline if active.');
          if (websocket && websocket.readyState === WebSocket.OPEN) {
            if (isVideoPipelineActive) {
              websocket.send('STOP_VIDEO');
              isVideoPipelineActive = false;
              videoPausedForHiddenTab = true;
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
        }, 250);
      }
    } else {
      if (hiddenVideoStopTimer !== null) { clearTimeout(hiddenVideoStopTimer); hiddenVideoStopTimer = null; }
      if (!videoPausedForHiddenTab && isVideoPipelineActive) armVisibleFrameProbe();
      if (videoPausedForHiddenTab) {
        videoPausedForHiddenTab = false;
        console.log('Tab is visible, resuming the video pipeline paused on hide.');
        if (websocket && websocket.readyState === WebSocket.OPEN) {
          websocket.send('START_VIDEO');
          isVideoPipelineActive = true;
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
      if (wakeLockSentinel === null) {
        console.log('Tab is visible again, re-acquiring Wake Lock.');
        await requestWakeLock();
      }
    }
  });

  /**
   * Decodes a JPEG stripe and queues it for the paint loop: ImageDecoder
   * (WebCodecs) where the context is secure, createImageBitmap elsewhere; both
   * yield an image the render and cleanup paths handle alike.
   * @param {number} startY
   * @param {ArrayBuffer} jpegData
   * @param {number} frameId
   */
  async function decodeAndQueueJpegStripe(startY, jpegData, frameId) {
    jpegStripeDecodesPending++;
    try {
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
    } finally {
      jpegStripeDecodesPending--;
    }
  }

  let paintScheduled = false;
  /**
   * Schedules the next paint tick on one rAF chain; starting the loop again
   * (a reconnect) must never create a second permanent chain.
   */
  function schedulePaintVideoFrame() {
    if (paintScheduled) return;
    paintScheduled = true;
    requestAnimationFrame(() => {
      paintScheduled = false;
      paintVideoFrame();
    });
  }

  /**
   * The per-rAF paint tick. Full-frame h264enc presents only the newest queued
   * frame; the striped modes composite their stripes and present the whole
   * frame as soon as its last row lands (the server emits a frame's stripes
   * in ascending order, so the last row proves it complete) or the socket and
   * the decoders go quiet (the stripe clock), falling back to presenting at
   * frame-id boundaries while stripes still flow; JPEG skips stripes that
   * decoded out of order; the shared main decoder path keeps the adaptive
   * jitter cushion, closing everything older than it in one tick because
   * draining one per rAF would let a burst back up the decoder's bounded
   * output pool. Leaving a full-frame mode tears both video sinks down
   * symmetrically, or a worker canvas would stay shown over the striped
   * content.
   */
  function paintVideoFrame() {
    if (!canvas || !canvasContext) {
      schedulePaintVideoFrame();
      return;
    }

    if (mstgActive || videoWorkerActive) {
      const fullFrameMode = (currentEncoderMode !== 'jpeg' && currentEncoderMode !== 'h264enc-striped');
      if (mstgActive && !fullFrameMode) deactivateMstg();
      // Diverted striped modes present through the worker sink; only an
      // undiverted striped mode reclaims the page canvas from it.
      if (videoWorkerActive && !fullFrameMode && !videoDivertOn) deactivateVideoWorker();
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

    let jpegPaintedThisFrame = false;

    if (currentEncoderMode === 'h264enc') {
      let paintedSomethingThisCycle = false;
      if (decodedStripesQueue.length > 0) {
        // Index math rather than repeated shift(), which re-indexes the array each time.
        const lastIdx = decodedStripesQueue.length - 1;
        for (let i = 0; i < lastIdx; i++) {
          try { decodedStripesQueue[i].frame.close(); } catch (e) {}
        }
        const frame = decodedStripesQueue[lastIdx].frame;
        decodedStripesQueue.length = 0;
        if (supportsWindowMSTG && presentFrameToVideo(frame)) {
          // Handed to the main-thread track generator.
        } else if (USE_OFFSCREEN_WORKER && presentFrameToWorker(frame)) {
          // Handed to the worker sink.
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
    } else if (currentEncoderMode === 'h264enc-striped') {
      if (USE_OFFSCREEN_WORKER && !videoWorker && !workerDecodeFailed && videoWorkerSpawnAttempts < 3) {
        ensureVideoWorker();
      }
      let paintedSomethingThisCycle = false;
      const ready = stripeCompositeBegin();
      const drained = stripeDecodesDrained();
      const settled = drained && stripeClock.settled();
      let bottomDrawn = false;
      if (ready) {
        for (const stripeData of decodedStripesQueue) {
          const fid = stripeData.frameId;
          if (!settled && stripePendingFrameId !== null && fid !== stripePendingFrameId && stripePendingDirty) {
            stripeCompositePresent();
            stripePendingDirty = false;
            paintedSomethingThisCycle = true;
          }
          stripePendingFrameId = fid;
          if (stripeData.yPos + stripeData.frame.displayHeight >= canvas.height) bottomDrawn = true;
          stripeCompositeDraw(stripeData.frame, stripeData.yPos);
          stripePendingDirty = true;
        }
      } else {
        for (const stripeData of decodedStripesQueue) { try { stripeData.frame.close(); } catch (e) {} }
      }
      decodedStripesQueue = [];
      if (drained && (settled || bottomDrawn) && stripePendingDirty
          && canvas.width > 0 && canvas.height > 0) {
        stripeCompositePresent();
        stripePendingDirty = false;
        paintedSomethingThisCycle = true;
      }
      if (paintedSomethingThisCycle && !streamStarted) {
        startStream();
      }
    } else if (currentEncoderMode === 'jpeg') {
      if (USE_OFFSCREEN_WORKER && !videoWorker && !workerDecodeFailed && videoWorkerSpawnAttempts < 3) {
        ensureVideoWorker();
      }
      const drained = jpegStripeDecodesPending === 0;
      const settled = drained && stripeClock.settled();
      let bottomDrawn = false;
      if (canvasContext && jpegStripeRenderQueue.length > 0) {
        if ((canvas.width === 0 || canvas.height === 0) || (canvas.width === 300 && canvas.height === 150)) {
          const firstStripe = jpegStripeRenderQueue[0];
          const firstHeight = firstStripe && firstStripe.image
            && (firstStripe.image.displayHeight ?? firstStripe.image.height);
          const firstWidth = firstStripe && firstStripe.image
            && (firstStripe.image.displayWidth ?? firstStripe.image.width);
          if (firstStripe && firstStripe.image
              && (firstStripe.startY + firstHeight > canvas.height || firstWidth > canvas.width)) {
            console.warn(`[paintVideoFrame] Canvas dimensions (${canvas.width}x${canvas.height}) may be too small for JPEG stripes.`);
          }
        }
        const ready = stripeCompositeBegin();
        while (jpegStripeRenderQueue.length > 0) {
          const segment = jpegStripeRenderQueue.shift();
          if (segment && segment.image) {
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
              if (ready) {
                if (!settled && segFrameId !== undefined && stripePendingFrameId !== null &&
                    segFrameId !== stripePendingFrameId && stripePendingDirty) {
                  stripeCompositePresent();
                  stripePendingDirty = false;
                }
                if (segFrameId !== undefined) stripePendingFrameId = segFrameId;
                const stripeHeight = segment.image.displayHeight ?? segment.image.height;
                if (segment.startY + stripeHeight >= canvas.height) bottomDrawn = true;
                stripeCompositeDraw(segment.image, segment.startY);
                stripePendingDirty = true;
              } else {
                try { segment.image.close(); } catch (closeError) { /* ignore */ }
              }
              if (segFrameId !== undefined) {
                lastDrawnJpegStripeFrameId[segment.startY] = segFrameId;
              }
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
          if (!streamStarted) {
            startStream();
            if (!inputInitialized && !isSharedMode) initializeInput();
          }
        }
      }
      if (drained && (settled || bottomDrawn) && stripePendingDirty && canvasContext
          && canvas.width > 0 && canvas.height > 0) {
        stripeCompositePresent();
        stripePendingDirty = false;
      }
    }
    schedulePaintVideoFrame();
  }

  /**
   * Builds the playback pipeline on the primary display: a 48 kHz
   * AudioContext, the AudioWorklet that queues decoded PCM (drop-oldest at
   * its cap, zero-fill on underrun, reporting depth and concealment counters
   * on request), a gain node for volume, and the Opus decode worker, widened
   * to the surround layout when the server streams one (best effort: the
   * browser still downmixes to the device's layout).
   */
  /**
   * Connects the decoder worker straight to the playback worklet.
   *
   * Decoded packets otherwise pass through the page to reach the worklet, so
   * anything occupying its thread -- a dashboard re-render, a `getUserMedia`
   * prompt -- stops playback being fed and the queue drains into concealment.
   * Called whenever either end appears; the page's own relay stays as the
   * fallback for a build where the channel never gets made.
   */
  function wireDecoderToWorklet() {
    if (!audioDecoderWorker || !audioWorkletProcessorPort) return;
    const channel = new MessageChannel();
    try {
      audioWorkletProcessorPort.postMessage({ type: 'pcmPort', port: channel.port1 }, [channel.port1]);
      audioDecoderWorker.postMessage({ type: 'pcmPort', port: channel.port2 }, [channel.port2]);
    } catch (e) {
      console.warn('Could not connect the audio decoder to the worklet directly:', e);
    }
  }

  /**
   * Connects the socket worker to the decoder, completing a path that reaches
   * playback without the page's thread on it at any point.
   */
  function wireSocketToDecoder() {
    if (!audioDecoderWorker || !websocket || typeof websocket.connectAudio !== 'function') return;
    const channel = new MessageChannel();
    try {
      audioDecoderWorker.postMessage({ type: 'audioIn', port: channel.port1 }, [channel.port1]);
      websocket.connectAudio(channel.port2);
    } catch (e) {
      console.warn('Could not connect the socket worker to the audio decoder:', e);
    }
  }

/**
 * Worker that owns the session socket.
 *
 * The page's thread is otherwise between the socket and the audio decoder, so
 * anything occupying it -- a dashboard re-render, a `getUserMedia` prompt --
 * stops audio being delivered for as long as it lasts. Here the socket is read
 * off that thread: `0x01` goes straight down a port to the decoder, and
 * everything else is handed to the page unchanged. Sends arrive from the page
 * and keep their order, since one port delivers in sequence. Gecko still
 * routes a worker's WebSocket delivery through the page's main thread, so
 * there a stall costs what the playback worklet's jitter depth cannot cover;
 * Chromium and WebKit deliver to the worker directly.
 */
const SOCKET_WORKER_SRC = `
let ws = null, audioPort = null, audioOn = true, primary = true;
let lastTick = 0;

// Encoded webcam frames sent while the socket was down would corrupt the
// server's decoder as deltas; held back until a keyframe restores the chain.
let webcamChainBroken = false;
// Video divert: 0x03/0x04 go straight to the video worker while the page
// keeps this on, and the newest frame id is acked from here on the same
// cadence the page would use, so pacing and RTT stay honest through a stall.
// Full frames ack on receipt; the striped modes ack the id the video worker
// reports presented, so a client that cannot render sheds load.
let videoPort = null, videoDivert = false, videoAck = false, videoAckSource = 'receive';
let videoLastId = -1, videoLastIdAt = 0, videoAckedId = -1, videoAckTimer = null;

// The page gates its own sends on what this reports, so a socket left holding
// bytes has to be reported as it drains: reporting only on send would freeze
// the page's view at the moment of a message too big for its gate, and nothing
// would ever send again to correct it.
let bufferedTimer = null;
function reportBuffered() {
  self.postMessage({ type: 'buffered', amount: ws ? ws.bufferedAmount : 0 });
  if (!ws || ws.bufferedAmount === 0) {
    if (bufferedTimer) { clearInterval(bufferedTimer); bufferedTimer = null; }
    return;
  }
  if (bufferedTimer) return;
  bufferedTimer = setInterval(() => {
    self.postMessage({ type: 'buffered', amount: ws ? ws.bufferedAmount : 0 });
    if (!ws || ws.bufferedAmount === 0) { clearInterval(bufferedTimer); bufferedTimer = null; }
  }, ${BUFFERED_DRAIN_MS});
}

function syncVideoAckTimer() {
  const want = videoDivert && videoAck;
  if (want && !videoAckTimer) {
    videoAckTimer = setInterval(() => {
      if (videoLastId < 0 || videoLastId === videoAckedId) return;
      if (!ws || ws.readyState !== 1) return;
      // The hold: how long the id waited on this tick, which a backgrounded
      // tab clamps to a second. The server subtracts it, so the round trip it
      // reports stays the path's rather than this timer's.
      const held = Math.max(0, Math.round(performance.now() - videoLastIdAt));
      try { ws.send('CLIENT_FRAME_ACK ' + videoLastId + ' ' + held); videoAckedId = videoLastId; } catch (err) {}
    }, ${BACKPRESSURE_INTERVAL_MS});
  } else if (!want && videoAckTimer) {
    clearInterval(videoAckTimer);
    videoAckTimer = null;
  }
}

self.onmessage = (e) => {
  const m = e.data;
  if (m.type === 'audioPort') { audioPort = m.port; return; }
  if (m.type === 'audioState') { audioOn = !!m.active; return; }
  if (m.type === 'videoPort') {
    videoPort = m.port;
    videoPort.onmessage = (ev) => {
      const pm = ev.data;
      if (pm && pm.presentedId !== undefined) { videoLastId = pm.presentedId; videoLastIdAt = performance.now(); }
    };
    return;
  }
  if (m.type === 'videoState') {
    videoDivert = !!m.divert;
    videoAck = !!m.ack;
    videoAckSource = m.ackSource || 'receive';
    syncVideoAckTimer();
    return;
  }
  if (m.type === 'sendPort') {
    // Fully framed messages from another worker (the microphone encoder).
    m.port.onmessage = (ev) => {
      if (!ws || ws.readyState !== 1) return;
      try { ws.send(ev.data); } catch (err) { /* onclose reports */ }
    };
    return;
  }
  if (m.type === 'webcamPort') {
    m.port.onmessage = (ev) => {
      const f = ev.data;
      if (!ws || ws.readyState !== 1) { webcamChainBroken = true; return; }
      if (webcamChainBroken && !f.keyframe) {
        try { ev.target.postMessage({ needKeyframe: true }); } catch (err) {}
        return;
      }
      const msg = new Uint8Array(3 + f.buffer.byteLength);
      msg[0] = 0x06;
      msg[1] = f.codecId;
      msg[2] = (f.keyframe ? 0x01 : 0x00) | ((((f.rotation || 0) / 90) & 0x03) << 1) | (f.flip ? 0x08 : 0x00);
      msg.set(new Uint8Array(f.buffer), 3);
      try {
        ws.send(msg.buffer);
        if (f.keyframe) webcamChainBroken = false;
      } catch (err) { webcamChainBroken = true; }
    };
    return;
  }
  if (m.type === 'open') {
    primary = m.primary !== false;
    ws = new WebSocket(m.url);
    ws.binaryType = 'arraybuffer';
    ws.onopen = () => self.postMessage({ type: 'open' });
    ws.onerror = () => self.postMessage({ type: 'error' });
    ws.onclose = (ev) => {
      self.postMessage({ type: 'close', code: ev.code, reason: ev.reason, wasClean: ev.wasClean });
      ws = null;
    };
    ws.onmessage = (ev) => {
      const d = ev.data;
      if (audioPort && audioOn && primary && d instanceof ArrayBuffer &&
          d.byteLength > 2 && new Uint8Array(d, 0, 1)[0] === 0x01) {
        // The page still owns the AudioContext, which only it can resume, so it
        // is told audio is arriving -- rarely, since this runs per packet.
        const now = Date.now();
        if (now - lastTick > 1000) { lastTick = now; self.postMessage({ type: 'audioTick' }); }
        audioPort.postMessage({ buffer: d }, [d]);
        return;
      }
      if (videoPort && videoDivert && d instanceof ArrayBuffer && d.byteLength > 6) {
        const t = new Uint8Array(d, 0, 1)[0];
        if (t === 0x03 || (t === 0x04 && d.byteLength > 10)) {
          if (videoAckSource === 'receive') {
            const head = new Uint8Array(d, 2, 2);
            videoLastId = (head[0] << 8) | head[1];
            videoLastIdAt = performance.now();
          }
          videoPort.postMessage(d, [d]);
          return;
        }
      }
      if (d instanceof ArrayBuffer) self.postMessage({ type: 'message', data: d }, [d]);
      else self.postMessage({ type: 'message', data: d });
    };
    return;
  }
  if (!ws) return;
  if (m.type === 'send') {
    try { ws.send(m.data); } catch (err) { /* a closing socket reports through onclose */ }
    reportBuffered();
    return;
  }
  if (m.type === 'close') { try { ws.close(m.code, m.reason); } catch (err) {} return; }
};
`;

/**
 * A `WebSocket`-shaped handle onto the socket the worker owns.
 *
 * Every call site keeps the API it had; only the thread the bytes are read on
 * changes. `bufferedAmount` is the worker's last report plus what has been
 * handed over since, so the backpressure gates that read it never see less
 * than the socket really holds.
 */
class WorkerWebSocket {
  /**
   * @param {string} url Session socket URL, query string included.
   * @param {boolean} primary Whether this page owns the primary display; only
   *     that one takes the audio short-circuit.
   */
  constructor(url, primary) {
    this.readyState = WebSocket.CONNECTING;
    this.binaryType = 'arraybuffer';
    this.onopen = this.onmessage = this.onerror = this.onclose = null;
    /** @type {Object<string, Array<function(*): void>>} */
    this._listeners = {};
    this._buffered = 0;
    const blob = new Blob([SOCKET_WORKER_SRC], { type: 'text/javascript' });
    const workerURL = URL.createObjectURL(blob);
    this._worker = new Worker(workerURL);
    URL.revokeObjectURL(workerURL);
    this._worker.onmessage = (e) => {
      const m = e.data;
      if (m.type === 'message') {
        const ev = { data: m.data };
        if (this.onmessage) this.onmessage(ev);
        this._emit('message', ev);
        return;
      }
      if (m.type === 'buffered') { this._buffered = m.amount; return; }
      if (m.type === 'audioTick') {
        if (audioContext && audioContext.state !== 'running') {
          audioContext.resume().catch(() => {});
        }
        return;
      }
      if (m.type === 'open') {
        this.readyState = WebSocket.OPEN;
        if (this.onopen) this.onopen();
        this._emit('open', {});
        return;
      }
      if (m.type === 'error') { if (this.onerror) this.onerror(m); this._emit('error', m); return; }
      if (m.type === 'close') {
        this.readyState = WebSocket.CLOSED;
        if (this.onclose) this.onclose(m);
        this._emit('close', m);
        this._retire();
        return;
      }
    };
    this._worker.postMessage({ type: 'open', url, primary });
  }

  /**
   * Bytes the socket still holds: the worker's last report plus everything
   * handed over since, so a backpressure gate never reads low.
   * @returns {number}
   */
  get bufferedAmount() { return this._buffered; }

  /**
   * @param {string} type One of `open`, `message`, `error`, `close`.
   * @param {function(*): void} fn
   * @returns {void}
   */
  addEventListener(type, fn) {
    (this._listeners[type] || (this._listeners[type] = [])).push(fn);
  }

  /**
   * @param {string} type
   * @param {function(*): void} fn
   * @returns {void}
   */
  removeEventListener(type, fn) {
    const list = this._listeners[type];
    if (!list) return;
    const at = list.indexOf(fn);
    if (at >= 0) list.splice(at, 1);
  }

  /**
   * Calls every listener for `type`; a throwing listener does not stop the rest.
   * @param {string} type
   * @param {*} ev
   * @returns {void}
   */
  _emit(type, ev) {
    const list = this._listeners[type];
    if (!list) return;
    for (const fn of list.slice()) {
      try { fn(ev); } catch (e) { /* a listener's error is its own */ }
    }
  }

  /**
   * Hands the decoder its line in, so audio never reaches the page thread.
   * @param {MessagePort} port Transferred to the worker.
   * @returns {void}
   */
  connectAudio(port) {
    try { this._worker.postMessage({ type: 'audioPort', port }, [port]); } catch (e) { /* no decoder yet */ }
  }

  /**
   * Mirrors the page's audio-pipeline flag, which gates the short-circuit.
   * @param {boolean} active
   * @returns {void}
   */
  setAudioActive(active) {
    try { this._worker.postMessage({ type: 'audioState', active }); } catch (e) { /* closing */ }
  }

  /**
   * Hands the worker a line that carries fully framed wire messages from
   * another worker (the microphone encoder), so sends skip the page thread.
   * @param {MessagePort} port Transferred to the worker.
   * @returns {void}
   */
  connectSend(port) {
    try { this._worker.postMessage({ type: 'sendPort', port }, [port]); } catch (e) { /* closing */ }
  }

  /**
   * Hands the worker the webcam encoder's line; the worker frames each
   * encoded chunk itself and answers `{needKeyframe: true}` when a send gap
   * broke the delta chain.
   * @param {MessagePort} port Transferred to the worker.
   * @returns {void}
   */
  connectWebcam(port) {
    try { this._worker.postMessage({ type: 'webcamPort', port }, [port]); } catch (e) { /* closing */ }
  }

  /**
   * Hands the worker the video worker's line in, for the full-frame divert.
   * @param {MessagePort} port Transferred to the worker.
   * @returns {void}
   */
  connectVideo(port) {
    try { this._worker.postMessage({ type: 'videoPort', port }, [port]); } catch (e) { /* closing */ }
  }

  /**
   * Turns the video divert on or off; while on, the worker also acks the
   * newest frame id itself unless this page is a shared viewer, whose acks
   * the server ignores. Full frames ack on receipt; the striped modes ack
   * what the video worker reports presented.
   * @param {boolean} divert
   * @param {string} [ackSource] `receive` (default) or `present`.
   * @returns {void}
   */
  setVideoDivert(divert, ackSource) {
    const ack = !!divert && displayId === 'primary' && !isSharedMode;
    try { this._worker.postMessage({ type: 'videoState', divert: !!divert, ack, ackSource: ackSource || 'receive' }); } catch (e) { /* closing */ }
  }

  /**
   * Queues one message on the socket. An ArrayBuffer is transferred rather
   * than copied, so a caller must not keep it; a view is copied.
   * @param {string|ArrayBuffer|ArrayBufferView} data
   * @returns {void}
   */
  send(data) {
    if (this.readyState !== WebSocket.OPEN) return;
    if (typeof data === 'string') {
      this._buffered += data.length;
      this._worker.postMessage({ type: 'send', data });
      return;
    }
    const buf = data instanceof ArrayBuffer ? data
      : ArrayBuffer.isView(data)
        ? data.buffer.slice(data.byteOffset, data.byteOffset + data.byteLength)
        : data;
    this._buffered += buf.byteLength || 0;
    this._worker.postMessage({ type: 'send', data: buf }, [buf]);
  }

  /**
   * @param {number} [code] Close code.
   * @param {string} [reason] Close reason.
   * @returns {void}
   */
  close(code, reason) {
    this.readyState = WebSocket.CLOSING;
    try { this._worker.postMessage({ type: 'close', code, reason }); } catch (e) { /* already gone */ }
    this._retire();
  }

  /**
   * Ends the worker once, whichever side closed the socket.
   * @returns {void}
   */
  _retire() {
    if (this._retiring) return;
    this._retiring = true;
    setTimeout(() => { try { this._worker.terminate(); } catch (e) { /* already gone */ } }, 2000);
  }
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
      // Detached first so nothing adopts it while it closes its own decoder.
      const outgoingAudioWorker = audioDecoderWorker;
      audioDecoderWorker = null;
      outgoingAudioWorker.postMessage({ type: 'close' });
      await new Promise(resolve => setTimeout(resolve, 50));
      outgoingAudioWorker.terminate();
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

                // Adaptive jitter depth under a fixed drop-oldest ceiling.
                // Output starts once target packets are queued; each
                // mid-stream underrun deepens the target by one, a clean
                // stretch decays it back, and standing depth above it is
                // trimmed a packet at a time -- so steady latency sits at the
                // smallest depth the delivery path has recently proven to
                // hold, and a jittery one (a stall upstream, Gecko routing
                // the socket through the page's thread) buys the depth it
                // demonstrably needs.
                this.TARGET_MIN = 2;
                this.TARGET_MAX = 6;
                this.MAX_BUFFER_PACKETS = 8;
                this.target = this.TARGET_MIN;
                this.priming = true;
                this.overCount = 0;
                this.cleanCount = 0;
                // Packets left at the moment one is pulled, tracked at its
                // minimum: the slack that proves a shallower target safe.
                this.shiftSlackMin = Infinity;

                // Concealment counters: zero-filled samples output on underrun, and
                // packets dropped by the drop-oldest ring when the queue overflows.
                this.underrunSamples = 0;
                this.droppedOldest = 0;
                // Output RMS accumulator (channel 0), reported with each stats reply.
                this._levelAcc = 0;
                this._levelCount = 0;

                this.enqueue = (buffer) => {
                    const pcmData = new Float32Array(buffer);
                    if (this.audioBufferQueue.length >= this.MAX_BUFFER_PACKETS) {
                        this.audioBufferQueue.shift();
                        this.droppedOldest++;
                    }
                    this.audioBufferQueue.push(pcmData);
                };
                this.port.onmessage = (event) => {
                    if (event.data.audioData) {
                        this.enqueue(event.data.audioData);
                    } else if (event.data.type === 'pcmPort' && event.data.port) {
                        // The decoder worker's own line in: decoded packets then
                        // reach this processor whatever the page's thread is doing.
                        event.data.port.onmessage = (m) => {
                            if (m.data && m.data.audioData) this.enqueue(m.data.audioData);
                        };
                    } else if (event.data.type === 'getBufferSize') {
                        const bufferMillis = this.audioBufferQueue.reduce((total, buf) => total + (buf.length / this.channels / sampleRate) * 1000, 0);
                        const level = this._levelCount > 0 ? Math.sqrt(this._levelAcc / this._levelCount) : 0;
                        this._levelAcc = 0;
                        this._levelCount = 0;
                        this.port.postMessage({
                            type: 'audioBufferSize',
                            size: this.audioBufferQueue.length,
                            durationMs: bufferMillis,
                            underrunSamples: this.underrunSamples,
                            droppedOldest: this.droppedOldest,
                            level: level
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

                if (this.priming) {
                    if (this.audioBufferQueue.length < this.target) {
                        zeroFill(0);
                        return true;
                    }
                    this.priming = false;
                }

                if (this.audioBufferQueue.length === 0 && this.currentAudioData === null) {
                    zeroFill(0);
                    // Full-buffer concealment.
                    this.underrunSamples += samplesPerBuffer;
                    this._reprime();
                    return true;
                }

                let data = this.currentAudioData;
                let offset = this.currentDataOffset;

                for (let sampleIndex = 0; sampleIndex < samplesPerBuffer; sampleIndex++) {
                    if (!data || offset >= data.length) {
                        if (this.audioBufferQueue.length > 0) {
                            const slack = this.audioBufferQueue.length - 1;
                            if (slack < this.shiftSlackMin) this.shiftSlackMin = slack;
                            data = this.currentAudioData = this.audioBufferQueue.shift();
                            offset = this.currentDataOffset = 0;
                        } else {
                            this.currentAudioData = null;
                            this.currentDataOffset = 0;
                            zeroFill(sampleIndex);
                            // Partial concealment.
                            this.underrunSamples += (samplesPerBuffer - sampleIndex);
                            this._reprime();
                            return true;
                        }
                    }

                    for (let c = 0; c < chans; c++) {
                        output[c][sampleIndex] = offset < data.length ? data[offset++] : output[0][sampleIndex];
                    }
                    const s0 = output[0][sampleIndex];
                    this._levelAcc += s0 * s0;
                    this._levelCount++;
                }

                this.currentDataOffset = offset;
                if (data && offset >= data.length) {
                    this.currentAudioData = null;
                    this.currentDataOffset = 0;
                }

                // Reclaims latency: depth held above target is a standing
                // delay, dropped one packet per window; long clean runs
                // shrink the target.
                if (this.audioBufferQueue.length > this.target) {
                    if (++this.overCount >= 250) {
                        this.audioBufferQueue.shift();
                        this.droppedOldest++;
                        this.overCount = 0;
                    }
                } else {
                    this.overCount = 0;
                }
                if (++this.cleanCount >= 2000) {
                    this.cleanCount = 0;
                    // Decay only over proven slack: a whole packet must have
                    // stayed spare at every pull, else a shallower target is
                    // a periodic audible probe rather than a reclaim.
                    if (this.target > this.TARGET_MIN && this.shiftSlackMin >= 2) this.target--;
                    this.shiftSlackMin = Infinity;
                }

                return true;
            }

            /** Restarts priming after an underrun, one packet deeper. */
            _reprime() {
                this.priming = true;
                this.target = Math.min(this.target + 1, this.TARGET_MAX);
                this.overCount = 0;
                this.cleanCount = 0;
                this.shiftSlackMin = Infinity;
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
      wireDecoderToWorklet();
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
            if (event.data.level !== undefined) {
              // Output RMS as a 0 to 100 level for the dashboards' audio meter.
              window.currentAudioLevel = Math.min(100, Math.round(event.data.level * 141));
            }
        }
      };
      audioGainNode = audioContext.createGain();
      audioGainNode.gain.value = currentVolume;
      audioWorkletNode.connect(audioGainNode);
      audioGainNode.connect(audioContext.destination);
      console.log('Playback AudioWorkletProcessor initialized and connected through a GainNode for volume control.');
      await applyOutputDevice();

      const audioDecoderWorkerBlob = new Blob([audioDecoderWorkerCode], {
        type: 'application/javascript'
      });
      const audioDecoderWorkerURL = URL.createObjectURL(audioDecoderWorkerBlob);
      audioDecoderWorker = new Worker(audioDecoderWorkerURL);
      URL.revokeObjectURL(audioDecoderWorkerURL);
      wireDecoderToWorklet();
      wireSocketToDecoder();
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

  /** Reinitializes the audio decoder in its worker, building the whole pipeline first if it is missing. */
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
      // The role and slot ride as query parameters; a fragment never reaches the server.
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
  // Under /api like the signaling socket, so one proxy rule covers everything.
  websocketEndpointURL.pathname += 'api/websockets';

  // `?socket_worker=false` keeps the socket on the page, which is also what a
  // policy forbidding blob workers leaves; the fallback below is the same path.
  const socketWorkerParam = urlParams.get('socket_worker');
  const socketWorkerEnabled = (socketWorkerParam !== null)
    ? (socketWorkerParam.toLowerCase() === 'true')
    : getBoolParam('socket_worker', true);
  try {
    if (!socketWorkerEnabled) throw new Error('socket_worker=false');
    websocket = new WorkerWebSocket(websocketEndpointURL.href, displayId === 'primary');
  } catch (e) {
    // No worker to be had (a policy forbidding blob workers, say). The socket
    // then runs here, where a busy thread costs audio its cadence, which is
    // still better than no session.
    if (socketWorkerEnabled) console.warn('[websockets] socket worker unavailable, reading on the page:', e);
    websocket = new WebSocket(websocketEndpointURL.href);
    websocket.binaryType = 'arraybuffer';
  }
  // The socket itself is in a worker, so this handle is what page-side
  // tooling has to observe or close the transport through.
  window.selkiesTransport = websocket;
  wireSocketToDecoder();
  // A fresh socket worker holds the divert default; re-point it.
  videoDivertOn = false;
  wireSocketToVideoWorker();

  /**
   * Acks the newest video frame the client is done with, so the server can
   * pace its sends against what this client actually keeps up with. The
   * striped modes composite on the page and ack what reached the screen; a
   * client whose rendering falls behind is then throttled instead of being
   * sent frames it will never show. Full-frame h264enc presents through sinks
   * the page cannot observe, so there the newest received id is the best the
   * client knows.
   */
  const sendBackpressureAck = () => {
    // While the divert runs, the socket worker acks the newest id itself, on
    // this same cadence but immune to a stalled page.
    if (videoDivertOn) return;
    if (websocket && websocket.readyState === WebSocket.OPEN) {
      try {
        const striped = (currentEncoderMode === 'jpeg' || currentEncoderMode === 'h264enc-striped');
        const fromPresent = striped && lastPresentedVideoFrameId !== null;
        const acked = fromPresent ? lastPresentedVideoFrameId : lastReceivedVideoFrameId;
        const at = fromPresent ? lastPresentedVideoFrameAt : lastReceivedVideoFrameAt;
        if (acked !== -1 && acked !== null) {
          // How long this id waited for the tick; the server subtracts it so a
          // throttled ack cadence is not reported as network latency.
          const held = Math.max(0, Math.round(performance.now() - at));
          websocket.send(`CLIENT_FRAME_ACK ${acked} ${held}`);
        }
      } catch (error) {
        console.error('[Backpressure] Error sending frame ACK:', error);
      }
    }
  };

  /**
   * Metrics tick: refreshes the audio buffer depth the backpressure gates
   * read and publishes `window.fps` — composites presented per second in the
   * striped modes, and the wire's frame ids per second for full-frame h264enc,
   * whose sinks present outside the page — independent of whether a dashboard
   * is open.
   */
  const sendClientMetrics = () => {
    if (audioWorkletProcessorPort) {
      audioWorkletProcessorPort.postMessage({
        type: 'getBufferSize'
      });
    }
    // A shared page keeps its local audio stats live but sends nothing.
    if (isSharedMode) return;

    const now = performance.now();
    const elapsedStriped = now - lastStripedFpsUpdateTime;
    const elapsedFullFrame = now - lastFpsUpdateTime;
    const fpsUpdateInterval = 1000;

    const wireFrameIds = uniqueStripedFrameIdsThisPeriod.size + divertedWireFramesThisPeriod;
    if (wireFrameIds > 0) {
      if (elapsedStriped >= fpsUpdateInterval) {
        const stripedFps = (wireFrameIds * 1000) / elapsedStriped;
        window.fps = Math.round(stripedFps);
        uniqueStripedFrameIdsThisPeriod.clear();
        divertedWireFramesThisPeriod = 0;
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

    retireCrashCountWhenHealthy();
  };

  /**
   * Sends the initial SETTINGS payload (every stored user pick, the client
   * geometry or manual resolution, the DPR-derived `scaling_dpi` seeded into
   * this very first payload so the desktop comes up at the right density
   * without a second capture restart, the display identity, the keyboard
   * layout and the audio-RED capability; a secondary sends only its own
   * suffixed per-display keys, never the primary's), advertises gzip,
   * requests the cache-only clipboard, and starts the metrics and ack timers.
   */
  websocket.onopen = async () => {
    console.log('[websockets] Connection opened!');
    await settleFullColorSupport();
    wsEverOpened = true;
    try { sessionStorage.removeItem('selkies_mode_flip'); } catch (e) { /* ignore */ }
    status = 'connected_waiting_mode';
    loadingText = 'Connection established. Waiting for server mode...';
    updateStatusDisplay();
    if (typeof DecompressionStream !== 'undefined') {
      try { websocket.send('_gz,1'); } catch (e) { /* handshake is best-effort */ }
    }
    window.postMessage({ type: 'trackpadModeUpdate', enabled: trackpadMode }, window.location.origin);
    if (!isSharedMode) {
      const settingsPrefix = `${storageAppName}_`;
      const settingsToSend = {};
      const dpr = useCssScaling ? 1 : (window.devicePixelRatio || 1);

      const knownSettings = [
        'framerate', 'video_crf', 'encoder', 'manual_resolution',
        'audio_bitrate', 'video_fullcolor', 'video_streaming_mode',
        'jpeg_quality', 'paint_over_jpeg_quality', 'use_cpu', 'video_paintover_crf',
        'video_paintover_burst_frames', 'use_paint_over_quality', 'scaling_dpi',
        'enable_binary_clipboard', 'rate_control_mode', 'video_bitrate',
        'force_aligned_resolution'
      ];
      const booleanSettingKeys = [
        'manual_resolution', 'video_fullcolor', 'video_streaming_mode',
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

          if (!isSpecific && displayId !== 'primary' && PER_DISPLAY_SETTINGS.includes(baseKey)) {
            continue;
          }
          if (knownSettings.includes(baseKey)) {
            let value = localStorage.getItem(key);
            if (booleanSettingKeys.includes(baseKey)) {
              value = (value === 'true');
            } else if (integerSettingKeys.includes(baseKey)) {
              value = parseInt(value, 10);
              if (isNaN(value)) continue;
            }
            settingsToSend[baseKey] = value;
          }
        }
      }

      if (manual_resolution && manual_width != null && manual_height != null) {
        settingsToSend['manual_resolution'] = true;
        settingsToSend['manual_width'] = alignResolution(manual_width);
        settingsToSend['manual_height'] = alignResolution(manual_height);
      } else {
        const videoContainer = document.querySelector('.video-container');
        const rect = videoContainer ? videoContainer.getBoundingClientRect() : {
          width: window.innerWidth,
          height: window.innerHeight
        };
        settingsToSend['manual_resolution'] = false;
        settingsToSend['initialClientWidth'] = alignResolution(rect.width * dpr);
        settingsToSend['initialClientHeight'] = alignResolution(rect.height * dpr);
      }

      if (settingsToSend['scaling_dpi'] === undefined) {
        settingsToSend['scaling_dpi'] = scalingDPI;
      }
      if (detectedKeyboardLayout) {
        settingsToSend['keyboardLayout'] = detectedKeyboardLayout;
      }
      settingsToSend['useCssScaling'] = useCssScaling;
      settingsToSend['displayId'] = displayId;
      settingsToSend['displayScale'] = currentDisplayScale(dpr);
      if (displayId === 'display2') {
          settingsToSend['displayPosition'] = displayPosition;
      }
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
    taggedClipboardFetch.armLegacyWindow(5000);
    websocket.send('cr');
    console.log('[websockets] Sent initial clipboard request (cr) to server (cache-only).');
    isVideoPipelineActive = true;
    isAudioPipelineActive = (displayId === 'primary');
    window.postMessage({
      type: 'pipelineStatusUpdate',
      video: true,
      audio: isAudioPipelineActive
    }, window.location.origin);

    if (metricsIntervalId === null) {
      metricsIntervalId = setInterval(sendClientMetrics, METRICS_INTERVAL_MS);
      console.log(`[websockets] Started client metrics every ${METRICS_INTERVAL_MS}ms.`);
    }
    if (!isSharedMode) {
        isMicrophoneActive = false;
        if (backpressureIntervalId === null) {
          backpressureIntervalId = setInterval(sendBackpressureAck, BACKPRESSURE_INTERVAL_MS);
          console.log(`[websockets] Started sending backpressure ACKs every ${BACKPRESSURE_INTERVAL_MS}ms.`);
        }
    }
  };

  /**
   * Order-preserving dispatch chain for received control text. Inflating a
   * 0x05 gzip frame is asynchronous, so control messages queue behind a
   * pending inflation to keep their arrival order (multipart clipboard
   * chunks); the chain is engaged only while one is pending, so the common
   * case stays synchronous. Media frames never queue: their frame ids order
   * them and the compression never touches them.
   */
  let __wsCtrlChain = Promise.resolve();
  let __wsGzPending = 0;
  const __inflateGz = async (buf) => {
    const stream = new Response(new Blob([buf]).stream().pipeThrough(new DecompressionStream('gzip')));
    return new TextDecoder().decode(await stream.arrayBuffer());
  };

  /**
   * Whether the server echoed `_gz,1`, after which text sends of 512 bytes or
   * more (clipboard) are gzipped into 0x05 frames through `websocket.send`,
   * patched below. Small text (input verbs) and binary (microphone, webcam)
   * are never wrapped, and a send chain keeps multipart chunks in sequence.
   */
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

  /**
   * Dispatches one message from the server (see the module docblock for the
   * framing): audio to the decode worker, JPEG stripes to the JPEG decoder,
   * H.264 to the worker, main or per-stripe decoder the mode selects, and
   * every control text to its handler. Every video chunk clears the
   * START_VIDEO watchdog and bumps `window.videoChunksReceived`, the "encoded
   * video ever arrived" signal the visibility probe reads.
   * @param {{data: ArrayBuffer|string}} event
   */
  const __rawWsMessage = (event) => {
    if (event.data instanceof ArrayBuffer) {
      const arrayBuffer = event.data;
      const dataView = new DataView(arrayBuffer);
      if (arrayBuffer.byteLength < 1) return;
      const dataTypeByte = dataView.getUint8(0);

      if (dataTypeByte === 0x03 || dataTypeByte === 0x04) {
        window.videoChunksReceived++;
        if (startVideoWatchdogTimer !== null) {
          clearStartVideoWatchdog();
        }
      }
      if (isSharedMode && (dataTypeByte === 0x03 || dataTypeByte === 0x04)) {
        lastSharedVideoChunkTime = performance.now();
        sharedStallRecoveryAttempts = 0;
        sharedStallNextRecoveryTime = 0;
      }

      if (dataTypeByte === 1) {
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
        const jpegHeaderLength = 6;
        if (arrayBuffer.byteLength < jpegHeaderLength) return;

        const jpegFrameId = dataView.getUint16(2, false);
        stripeClock.note(jpegFrameId);
        if (!isSharedMode) { lastReceivedVideoFrameId = jpegFrameId; lastReceivedVideoFrameAt = performance.now(); }
        const stripe_y_start = dataView.getUint16(4, false);
        const jpegDataBuffer = arrayBuffer.slice(jpegHeaderLength);

        const canProcessJpeg =
          (!isSharedMode && isVideoPipelineActive && currentEncoderMode === 'jpeg') ||
          (isSharedMode && currentEncoderMode === 'jpeg');

        if (canProcessJpeg) {
          if (jpegDataBuffer.byteLength === 0) return;
          decodeAndQueueJpegStripe(stripe_y_start, jpegDataBuffer, jpegFrameId);
        }

      } else if (dataTypeByte === 0x04) {
        const EXPECTED_HEADER_LENGTH = 10;
        if (arrayBuffer.byteLength < EXPECTED_HEADER_LENGTH) return;

        const video_frame_type_byte = dataView.getUint8(1);
        const vncFrameID = dataView.getUint16(2, false);
        stripeClock.note(vncFrameID);
        if (!isSharedMode) {
            lastReceivedVideoFrameId = vncFrameID;
            lastReceivedVideoFrameAt = performance.now();
            // Full-frame h264enc presents through sinks the page cannot count, so its
            // rate is measured off the wire; the striped modes count what they composite.
            if (currentEncoderMode !== 'h264enc-striped') {
                uniqueStripedFrameIdsThisPeriod.add(lastReceivedVideoFrameId);
            }
        }
        const vncStripeYStart = dataView.getUint16(4, false);
        const stripeWidth = dataView.getUint16(6, false);
        const stripeHeight = dataView.getUint16(8, false);
        const h264Payload = arrayBuffer.slice(EXPECTED_HEADER_LENGTH);

        // A shared viewer sends no SETTINGS, so the stream geometry is learned
        // from the frame header itself; the canvas box the sinks mirror follows.
        if (isSharedMode && currentEncoderMode !== 'h264enc-striped' &&
            stripeWidth > 0 && stripeHeight > 0 &&
            (manual_width !== stripeWidth || manual_height !== stripeHeight)) {
            manual_width = stripeWidth;
            manual_height = stripeHeight;
            console.log(`Shared mode: stream is ${manual_width}x${manual_height} (physical).`);
            applyManualCanvasStyle(manual_width, manual_height, true);
        }
        // Terminal, not the fallback ladder: reloading cannot conjure a
        // decoder, and a shared viewer has no JPEG pipeline to drop to.
        if (isSharedMode && typeof VideoDecoder === 'undefined') {
            if (!sharedDecoderUnavailableNoticed) {
                sharedDecoderUnavailableNoticed = true;
                console.error('Shared viewing needs WebCodecs VideoDecoder, which this browser lacks.');
                if (statusDisplayElement) {
                    statusDisplayElement.textContent = 'This browser cannot decode the shared stream (no WebCodecs).';
                    statusDisplayElement.classList.remove('hidden');
                }
            }
            return;
        }

        // A keyframe still in H.264 after the JPEG rung was asked for: the
        // server holds the encoder, and the refusal has nowhere left to go.
        if (!isSharedMode && codecRefusalAnswered && currentEncoderMode === 'jpeg'
            && video_frame_type_byte === 0x01 && h264Payload.byteLength > 0) {
            answerRefusedCodec(codecFromKeyframe(h264Payload, null)
                || getDynamicH264Codec(stripeWidth, stripeHeight, video_fullcolor, framerate));
            return;
        }

        if (decodeInWorker && currentEncoderMode === 'h264enc' && (isSharedMode || isVideoPipelineActive)) {
            if (h264Payload.byteLength === 0) return;
            if (video_frame_type_byte === 0x01) {
                const spsCodec = codecFromKeyframe(h264Payload, null);
                if (spsCodec && spsCodec !== workerKeyframeCodec) {
                    workerKeyframeCodec = spsCodec;
                }
            }
            const workerCodec = workerKeyframeCodec || getDynamicH264Codec(stripeWidth, stripeHeight, video_fullcolor, framerate);
            if (feedWorkerDecoder(video_frame_type_byte === 0x01, h264Payload, stripeWidth, stripeHeight, workerCodec)) {
                return;
            }
        }

        // Full-frame h264enc is the stripe path with a single decoder at row 0,
        // for a shared viewer exactly as for the primary.
        const canProcessVncStripe =
            (currentEncoderMode === 'h264enc' || currentEncoderMode === 'h264enc-striped') &&
            (isSharedMode || isVideoPipelineActive);

        if (canProcessVncStripe) {
            if (h264Payload.byteLength === 0) return;

            let decoderInfo = vncStripeDecoders[vncStripeYStart];
            const chunkType = (video_frame_type_byte === 0x01) ? 'key' : 'delta';
            const needKeyframe = !decoderInfo || !decoderInfo.hasReceivedKeyframe;
            if (chunkType === 'delta' && needKeyframe) {
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
                    error: (e) => handleStripeDecodeError(e, vncStripeYStart)
                });
                let dynamicCodec = getDynamicH264Codec(stripeWidth, stripeHeight, video_fullcolor, framerate);
                if (video_frame_type_byte === 0x01) {
                    dynamicCodec = codecFromKeyframe(h264Payload, dynamicCodec);
                }
                const decoderConfig = decoderConfigFor({
                    codec: dynamicCodec,
                    codedWidth: stripeWidth,
                    codedHeight: stripeHeight,
                    optimizeForLatency: true
                });
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
                            // The catch below closes the decoder while the map entry still points at it.
                            answerRefusedCodec(dynamicCodec);
                            const refusal = new Error(`config not supported: ${dynamicCodec}`);
                            refusal.quiet = true;
                            return Promise.reject(refusal);
                        }
                    })
                    .then(() => {
                        processPendingChunksForStripe(vncStripeYStart);
                    })
                    .catch(e => {
                        if (!e.quiet) console.error(`Error configuring VNC stripe decoder Y=${vncStripeYStart}:`, e);
                        if (vncStripeDecoders[vncStripeYStart] && vncStripeDecoders[vncStripeYStart].decoder === newStripeDecoder) {
                            try { if (newStripeDecoder.state !== 'closed') newStripeDecoder.close(); } catch (_) {}
                            delete vncStripeDecoders[vncStripeYStart];
                        }
                    });
            }

            if (decoderInfo) {
                if (chunkType === 'delta' && !decoderInfo.hasReceivedKeyframe) {
                    requestKeyframe();
                    return;
                }
                if (chunkType === 'key') {
                    decoderInfo.hasReceivedKeyframe = true;
                } else if (decoderInfo.decoder.decodeQueueSize > STRIPE_DECODE_QUEUE_LIMIT) {
                    decoderInfo.hasReceivedKeyframe = false;
                    requestKeyframe();
                    return;
                }
                // Striped H.264 carries the frame id in the timestamp so the paint
                // loop can present whole frames; full-frame keeps a monotonic clock.
                const chunkTimestamp = (currentEncoderMode === 'h264enc-striped')
                    ? vncFrameID : (performance.now() * 1000);
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
                    // A mismatched geometry is a straggler from the previous encoder
                    // mode; queued, it would fail later and trip the fallback reload.
                    if (decoderInfo.width && (decoderInfo.width !== stripeWidth || decoderInfo.height !== stripeHeight)) {
                        console.warn(`Dropping stale stripe chunk for Y=${vncStripeYStart}: ${stripeWidth}x${stripeHeight} vs decoder ${decoderInfo.width}x${decoderInfo.height}.`);
                        return;
                    }
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
                            if (document.hidden) {
                                // Hidden on connect: stays paused until the next tab-show.
                                sharedVideoPaused = true;
                                console.log("Shared mode: hidden on init, leaving video paused.");
                            } else {
                                websocket.send('START_VIDEO');
                                console.log("Shared mode: Sent START_VIDEO after initial STOP_VIDEO.");
                            }
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
            } else if (hash.startsWith('#player')) {
                clientRole = 'viewer'; clientSlot = parseInt(hash.substring(7), 10) || null;
                // #playerN addresses slot N, the same 0-based input target an assigned slot yields.
                if (clientSlot !== null) playerInputTargetIndex = clientSlot - 1;
            } else {
                clientRole = 'controller';
                clientSlot = 1;
                playerInputTargetIndex = 0;
            }
            console.log(`Legacy mode detected. Role from hash: ${clientRole}, Slot: ${clientSlot}`);
            initializeInput();
        }

        clearAllVncStripeDecoders();
        cleanupJpegStripeQueue();
        clearDecodedStripesQueue();

        if (!isSharedMode) {
            stopMicrophoneCapture();
            stopWebcamCapture();
            if (!isTokenAuthMode) {
                initializeInput();
            }
            // No main decoder here: only shared mode feeds it, and an idle one
            // would pin a scarce hardware decode session.
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

        schedulePaintVideoFrame();

        if (isSharedMode) {
            sharedClientState = 'ready';
            console.log("Shared mode: Received 'MODE websockets'. Requesting initial stream with STOP/START_VIDEO. State: ready.");
            armSharedStallWatchdog();
            if (websocket && websocket.readyState === WebSocket.OPEN) {
                 websocket.send('STOP_VIDEO');
                 setTimeout(() => {
                    if (websocket && websocket.readyState === WebSocket.OPEN) {
                        if (document.hidden) {
                            // Hidden on connect: stays paused until the next tab-show.
                            sharedVideoPaused = true;
                            console.log("Shared mode: hidden on init, leaving video paused.");
                        } else {
                            websocket.send('START_VIDEO');
                            console.log("Shared mode: Sent START_VIDEO after initial STOP_VIDEO.");
                        }
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
        if (firstFrameRecoveryTimer !== null) clearInterval(firstFrameRecoveryTimer);
        let firstFrameNudges = 0;
        firstFrameRecoveryTimer = setInterval(() => {
          if (streamStarted || !websocket || websocket.readyState !== WebSocket.OPEN || firstFrameNudges >= 5) {
            clearInterval(firstFrameRecoveryTimer);
            firstFrameRecoveryTimer = null;
            return;
          }
          firstFrameNudges++;
          console.log(`No frame since connect; requesting keyframe (attempt ${firstFrameNudges}).`);
          requestKeyframe();
        }, 3000);
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
          else if (obj.type === 'network_stats') {
            window.network_stats = obj;
            if (typeof obj.latency_ms === 'number') networkStat.latencyMs = obj.latency_ms;
            if (typeof obj.bandwidth_mbps === 'number') networkStat.bandwidthMbps = obj.bandwidth_mbps;
          }
          else if (obj.type === 'server_settings') {
              if (displayId !== 'primary' && obj.settings.second_screen && obj.settings.second_screen.value === false) {
                  console.error("The server reports no second display is available. This client will not function.");
                  if (statusDisplayElement) {
                      statusDisplayElement.textContent = 'Error: A second display is not available on this server.';
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
              if (typeof window['encoder'] === 'string' && !canDecodeEncoder(window['encoder'])) {
                  showUndecodableEncoderNotice(window['encoder']);
              } else if (typeof window['encoder'] === 'string' && window['encoder'] !== currentEncoderMode) {
                  const newEnc = window['encoder'];
                  clearUndecodableEncoderNotice();
                  console.log(`Server settings switch encoder ${currentEncoderMode} -> ${newEnc}.`);
                  currentEncoderMode = newEnc;
                  updateVideoDivert();
                  if (newEnc !== 'h264enc-striped') {
                      clearAllVncStripeDecoders();
                  }
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
              // The server-advertised value, not window.command_enabled, which
              // for an unlocked bool keeps the client's persisted value.
              const ce = obj.settings && obj.settings.command_enabled;
              serverCommandEnabled = (ce && typeof ce.value === 'boolean') ? ce.value : true;
              // Deployment policy the resize paths read; absent keeps resizing enabled.
              const er = obj.settings && obj.settings.enable_resize;
              if (er && typeof er.value === 'boolean') window.enable_resize = er.value;
              // The clipboard direction gates are deployment policy: the server value wins.
              const cin = obj.settings && obj.settings.clipboard_in_enabled;
              if (cin && typeof cin.value === 'boolean') clipboard_in_enabled = cin.value;
              const cout = obj.settings && obj.settings.clipboard_out_enabled;
              if (cout && typeof cout.value === 'boolean') clipboard_out_enabled = cout.value;
              const ebc = obj.settings && obj.settings.enable_binary_clipboard;
              if (ebc && typeof ebc.value === 'boolean') {
                enable_binary_clipboard = ebc.locked ? ebc.value : getBoolParam('enable_binary_clipboard', ebc.value);
              }
              const wce = obj.settings && obj.settings.webcam_encoder;
              if (wce && WEBCAM_ENCODER_PREFERENCES.includes(wce.value)) {
                const stored = getStringParam('webcam_encoder', wce.value);
                webcamEncoderPreference = wce.locked || !WEBCAM_ENCODER_PREFERENCES.includes(stored)
                  ? wce.value : stored;
              }
              const cssScaling = resolvedCssScaling(obj.settings);
              if (cssScaling !== useCssScaling) {
                  window.postMessage({ type: 'setUseCssScaling', value: cssScaling },
                                     window.location.origin);
              }
              // After the gates above, so the one-time initial push honours them.
              maybeSendInitialClipboard();
              window.postMessage({ type: 'serverSettings', payload: obj.settings }, window.location.origin);
              if (Object.keys(changes).length > 0) {
                  console.log('Client settings were sanitized by server rules. Sending updates back to server:', changes);
                  handleSettingsMessage(changes, true);
              }
              const serverForcesManual = obj.settings && obj.settings.manual_resolution && obj.settings.manual_resolution.value === true;

              if (serverForcesManual || window.manual_resolution) {
                  console.log(`Manual resolution mode active (Server forced: ${serverForcesManual}, Client pref: ${window.manual_resolution}). Switching to manual resize handlers.`);
                  if (serverForcesManual) {
                      const serverWidth = obj.settings.manual_width ? parseInt(obj.settings.manual_width.value, 10) : 0;
                      const serverHeight = obj.settings.manual_height ? parseInt(obj.settings.manual_height.value, 10) : 0;
                      if (serverWidth > 0 && serverHeight > 0) {
                          console.log(`Applying server-enforced manual resolution: ${serverWidth}x${serverHeight}`);
                          window.manual_resolution = true;
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
              if (!isVideoPipelineActive && (currentEncoderMode === 'h264enc' || currentEncoderMode === 'h264enc-striped') && !isSharedMode) {
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
              if (websocket && websocket.setAudioActive) websocket.setAudioActive(isAudioPipelineActive);
            }
            if (statusChanged) window.postMessage({
              type: 'pipelineStatusUpdate',
              video: isVideoPipelineActive,
              audio: isAudioPipelineActive
            }, window.location.origin);
         } else if (obj.type === 'stream_resolution') {
           // Another display's resolution would rescale this page's canvas and
           // input mapping; a server without the field only ever sent the primary's.
           const resolutionDisplayId = obj.displayId || 'primary';
           if (resolutionDisplayId !== displayId) {
             console.log(`Ignoring stream_resolution for display '${resolutionDisplayId}' (this page renders '${displayId}').`);
           } else if (isSharedMode) {
             if (sharedClientState === 'error' || sharedClientState === 'idle') {
               console.log(`Shared mode: Received stream_resolution while in state '${sharedClientState}'. Ignoring.`);
             } else {
               const physicalNewWidth = parseInt(obj.width, 10);
               const physicalNewHeight = parseInt(obj.height, 10);

               if (physicalNewWidth > 0 && physicalNewHeight > 0) {
                 // Shared-mode sizing works in physical stream pixels; the
                 // viewer's own DPR is unrelated to the controller's stream.
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
                   console.log(`Shared mode: Clearing decoders and canvas for new resolution.`);
                   clearAllVncStripeDecoders();
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
             const appliedWidth = parseInt(obj.width, 10);
             const appliedHeight = parseInt(obj.height, 10);
             if (appliedWidth > 0 && appliedHeight > 0) {
               // The realized resolution can differ from the request (encoder
               // alignment, RandR cell snapping, a rejected mode-set); canvas,
               // stripe decoders and input mapping follow it.
               const dprUsed = (window.manual_resolution || useCssScaling) ? 1 : (window.devicePixelRatio || 1);
               const bufferWidth = alignResolution(appliedWidth);
               const bufferHeight = alignResolution(appliedHeight);
               if (canvas && bufferWidth > 0 && bufferHeight > 0 &&
                   (canvas.width !== bufferWidth || canvas.height !== bufferHeight)) {
                 console.log(`Server realized stream resolution ${appliedWidth}x${appliedHeight} (canvas buffer ${canvas.width}x${canvas.height}); reconciling.`);
                 clearAllVncStripeDecoders();
                 // CSS times DPR no longer equals server pixels: input routes through the canvas box.
                 window.streamResolutionDiverged = true;
                 if (window.manual_resolution) {
                   manual_width = bufferWidth;
                   manual_height = bufferHeight;
                   applyManualCanvasStyle(manual_width, manual_height, scaleLocallyManual);
                 } else {
                   // +0.5 keeps the divide/multiply round trip on a fractional DPR
                   // from flooring one even step below the realized size.
                   applyManualCanvasStyle((bufferWidth + 0.5) / dprUsed, (bufferHeight + 0.5) / dprUsed, true);
                 }
               }
             } else {
               console.warn(`Received invalid stream_resolution dimensions: ${obj.width}x${obj.height}`);
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
        } else if (event.data.startsWith('clipboard_reply,')) {
            if (event.data.substring(16) === 'cr') armTaggedClipboardReply();
        } else if (event.data.startsWith('clipboard_start,')) {
            const parts = event.data.split(',');
            multipartClipboard.begin(parts[1], parseInt(parts[2], 10));
            console.log(`Starting multi-part clipboard download: ${multipartClipboard.mimeType}, total size: ${multipartClipboard.totalSize}`);
        } else if (event.data.startsWith('clipboard_data,')) {
            if (multipartClipboard.inProgress) {
                try {
                    // Handed to the worker as it arrives, so the page never
                    // holds the payload.
                    multipartClipboard.push(event.data.substring(15));
                } catch (e) {
                    console.error('Error processing multi-part clipboard chunk:', e);
                    multipartClipboard.reset();
                }
            }
        } else if (event.data === 'clipboard_finish') {
            if (multipartClipboard.inProgress) {
                console.log(`Finished multi-part clipboard download. Received ${multipartClipboard.receivedSize} of ${multipartClipboard.totalSize} bytes.`);
                if (multipartClipboard.receivedSize !== multipartClipboard.totalSize) {
                    console.error('Multipart clipboard size mismatch. Aborting.');
                    multipartClipboard.reset();
                } else {
                    // Consumed before the async decode so message order still
                    // defines which payload settles the connect-time fetch.
                    const isInitClipboardFetch = consumeInitClipboardFetch();
                    const mpMime = multipartClipboard.mimeType;
                    multipartClipboard.finish().then(({ result, hash, byteLength }) => {
                        if (mpMime === 'text/plain') {
                            const text = result;
                            // Checked before resolveServer records the signature.
                            const isFreshContent = clipboardSync.shouldSend(text, 'text/plain');
                            clipboardSync.resolveServer(text, null, 'text/plain');
                            if (!isInitClipboardFetch && clipboard_out_enabled && isFreshContent) {
                                deferredClipboardWriter.write(
                                    () => navigator.clipboard.writeText(text), {
                                        onFailure: (err) => console.error('Could not copy server clipboard text to local: ' + err),
                                    });
                            }
                            window.postMessage(clipboardPreviewMessage(text), window.location.origin);
                        } else if (clipboard_out_enabled && enable_binary_clipboard) {
                            const bytes = result;
                            const blob = new Blob([bytes], { type: mpMime });
                            const digest = digestedPayload(byteLength, hash);
                            const isFreshContent = clipboardSync.shouldSend(digest, mpMime);
                            clipboardSync.resolveServer(undefined, blob, mpMime, digest);
                            if (!isInitClipboardFetch && isFreshContent) {
                                deferredClipboardWriter.write(
                                    () => writeImageToLocalClipboard(blob, mpMime, reencodePngOffThread), {
                                        onSuccess: () => {
                                            console.log(`Successfully wrote multi-part image (${mpMime}) from server to local clipboard.`);
                                            clipboardSync.captureLocalImageSig();
                                            const uiText = `Image (${mpMime}) received from session and copied to clipboard.`;
                                            window.postMessage({ type: 'clipboardContentUpdate', text: uiText }, window.location.origin);
                                        },
                                        onFailure: notifyClipboardImageWriteFailed,
                                    });
                            }
                        }
                    }).catch((e) => {
                        console.error('Error assembling final clipboard content:', e);
                    });
                }
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
                // Consumed before the async decode, which runs in the worker.
                const isInitClipboardFetch = consumeInitClipboardFetch();
                clipboardWorker.decode(base64Data, mimeType).then(({ result, hash, byteLength }) => {
                    const bytes = result;
                    const blob = new Blob([bytes], { type: mimeType });
                    const digest = digestedPayload(byteLength, hash);
                    const isFreshContent = clipboardSync.shouldSend(digest, mimeType);
                    clipboardSync.resolveServer(undefined, blob, mimeType, digest);
                    if (isInitClipboardFetch) return;
                    if (!isFreshContent) return;
                    deferredClipboardWriter.write(
                        () => writeImageToLocalClipboard(blob, mimeType, reencodePngOffThread), {
                            onSuccess: () => {
                                console.log(`Successfully wrote image (${mimeType}) from server to local clipboard.`);
                                clipboardSync.captureLocalImageSig();
                                const uiText = `Image (${mimeType}) received from session and copied to clipboard.`;
                                window.postMessage({ type: 'clipboardContentUpdate', text: uiText }, window.location.origin);
                            },
                            onFailure: notifyClipboardImageWriteFailed,
                        });
                }).catch((e) => {
                    console.error('Error processing binary clipboard data from server:', e);
                });
            } catch (e) {
                console.error('Error processing binary clipboard data from server:', e);
            }
        } else if (event.data.startsWith('clipboard,')) {
          try {
            const base64Payload = event.data.substring(10);
            // Gated synchronously, since message order defines the connect-time fetch.
            const writeLocal = !consumeInitClipboardFetch() && clipboard_out_enabled;
            clipboardWorker.decode(base64Payload, 'text/plain').then(({ result }) => {
                const decodedText = result;
                const isFreshContent = clipboardSync.shouldSend(decodedText, 'text/plain');
                clipboardSync.resolveServer(decodedText, null, 'text/plain');
                if (writeLocal && isFreshContent) {
                    deferredClipboardWriter.write(
                        () => navigator.clipboard.writeText(decodedText), {
                            onFailure: (err) => console.error('Could not copy server clipboard to local: ' + err),
                        });
                }
                window.postMessage(clipboardPreviewMessage(decodedText), window.location.origin);
            }).catch((e) => {
                console.error('Error processing clipboard data:', e);
            });
          } catch (e) {
            console.error('Error processing clipboard data:', e);
          }
        } else if (event.data.startsWith('system,')) {
          try {
            const systemMsg = JSON.parse(event.data.substring(7));
            if (systemMsg.action === 'reload') window.location.reload();
            else if (typeof systemMsg.action === 'string' &&
                systemMsg.action.startsWith('command_error,') && !isSharedMode) {
              // Surfaced in the warning channel, or the optimistic UI reads as success.
              window.postMessage({
                type: 'fileUpload',
                payload: {
                  status: 'warning',
                  fileName: 'command',
                  message: systemMsg.action.slice('command_error,'.length),
                  code: 'commandFailed',
                },
              }, window.location.origin);
            }
            else if (typeof systemMsg.action === 'string' &&
                systemMsg.action.startsWith('command_done,') && !isSharedMode) {
              // Settles what the apps panel shows as running; a failure arrives
              // on the channel above instead.
              window.postMessage({
                type: 'commandDone',
                command: systemMsg.action.slice('command_done,'.length),
              }, window.location.origin);
            }
            else if (typeof systemMsg.action === 'string' &&
                systemMsg.action.startsWith('apps_installed,') && !isSharedMode) {
              // Broadcast when a command changed it: this page may not be the
              // one that ran it.
              window.postMessage({
                type: 'appsInstalled',
                apps: JSON.parse(systemMsg.action.slice('apps_installed,'.length)),
              }, window.location.origin);
            }
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

                latestDisplayLayouts = payload.layouts || null;
                if (window.webrtcInput && window.webrtcInput.setDisplayLayouts) {
                    window.webrtcInput.setDisplayLayouts(latestDisplayLayouts, displayId);
                }
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
          if (websocket && websocket.setAudioActive) websocket.setAudioActive(true);
        } else if (event.data === 'AUDIO_STOPPED' && !isSharedMode) {
          isAudioPipelineActive = false;
          window.postMessage({ type: 'pipelineStatusUpdate', audio: false }, window.location.origin);
          if (audioDecoderWorker) audioDecoderWorker.postMessage({ type: 'updatePipelineStatus', data: { isActive: false } });
          if (websocket && websocket.setAudioActive) websocket.setAudioActive(false);
        } else if (event.data === 'AUDIO_DISABLED' && !isSharedMode) {
          console.log("Server reports audio is disabled. Tearing down audio workers.");
          audioEnabled = false;
          isAudioPipelineActive = false;
          if (audioDecoderWorker) {
            audioDecoderWorker.postMessage({ type: 'updatePipelineStatus', data: { isActive: false } });
            if (websocket && websocket.setAudioActive) websocket.setAudioActive(false);
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
        } else if (event.data === 'WEBCAM_DISABLED' && !isSharedMode) {
          console.log("Server reports webcam is disabled. Stopping webcam capture.");
          webcamEnabled = false;
          stopWebcamCapture();
          window.postMessage({ type: 'pipelineStatusUpdate', webcam: false }, window.location.origin);
        } else if (event.data === 'WEBCAM_KEYFRAME') {
          // The server's decoder lost its reference or just started.
          if (webcamCapture) webcamCapture.requestKeyframe();
        } else {
          if (window.webrtcInput && window.webrtcInput.on_message && !isSharedMode) {
            window.webrtcInput.on_message(event.data);
          }
        }
      }
    }
  };

  /** Inflates 0x05 frames and routes everything through `__rawWsMessage` in order (see `__wsCtrlChain`). */
  websocket.onmessage = (event) => {
    const d = event.data;
    if (d instanceof ArrayBuffer) {
      if (d.byteLength >= 1 && new Uint8Array(d, 0, 1)[0] === 0x05) {
        __wsGzPending++;
        const gz = d.slice(1);
        __wsCtrlChain = __wsCtrlChain.then(async () => {
          try { __rawWsMessage({ data: await __inflateGz(gz) }); }
          catch (e) { console.error('[websockets] gzip control inflate failed:', e); }
          finally { __wsGzPending--; }
        });
        return;
      }
      __rawWsMessage(event);
      return;
    }
    if (d === '_gz,1') {
      if (typeof CompressionStream !== 'undefined') wsGzTx = true;
      return;
    }
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

  /**
   * Tears the session down and schedules a reconnect through a page reload,
   * except after an invalid token (4001) or when another live connection
   * superseded this one, where auto-reconnecting would evict the new holder
   * and the two pages would trade the session forever.
   */
  websocket.onclose = (event) => {
    console.log('[websockets] Connection closed', event);
    // The auth probe reloads the page when the origin now answers 401.
    if (window.__selkiesAuthProbe) window.__selkiesAuthProbe();
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
    const superseded = /superseded/i.test(event.reason || '');
    if (superseded) {
        console.warn("Session superseded by a new connection. Auto-reconnect disabled.");
        if (reconnectIntervalId) clearInterval(reconnectIntervalId);
        reconnectIntervalId = null;
    }
    status = 'disconnected';
    loadingText = superseded
      ? 'Session opened elsewhere. Reload this page to take over.'
      : 'WebSocket disconnected. Attempting to reconnect...';
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
    cleanupJpegStripeQueue();
    clearAllVncStripeDecoders();
    deactivateStripeWorker();
    if (audioDecoderWorker) {
      audioDecoderWorker.postMessage({
        type: 'close'
      });
      audioDecoderWorker = null;
    }
    if (!isSharedMode) { stopMicrophoneCapture(); stopWebcamCapture(); }
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
        clearSharedStallWatchdog();
    }
    if (!superseded && !reconnectIntervalId) {
      reconnectIntervalId = setInterval(() => {
        if (websocket && (websocket.readyState === WebSocket.OPEN || websocket.readyState === WebSocket.CONNECTING)) {
        } else {
          console.log("WebSocket disconnected, reloading page to reconnect.");
          reloadPossiblyFlippingMode();
        }
      }, 5000);
    }
  };
}

let wsEverOpened = false;

/**
 * Reloads the page, first switching the stored stream mode to WebRTC when the
 * server is serving that transport: a plain GET on the transport endpoint
 * answers 409 exactly then. One attempt per connect cycle, and only if this
 * session never connected, so a client whose stored mode disagrees with the
 * server converges instead of loop-reloading.
 */
async function reloadPossiblyFlippingMode() {
  let flipGuard = null;
  try { flipGuard = sessionStorage.getItem('selkies_mode_flip'); } catch (e) { /* ignore */ }
  if (!wsEverOpened && !flipGuard) {
    try {
      // The same path derivation as the data socket, so the probe hits its route.
      const probeURL = new URL(window.location.href);
      probeURL.pathname = getRoutePrefix() + '/api/websockets';
      const res = await fetch(probeURL.href, { cache: 'no-store', headers: sessionAuthHeaders() });
      if (res.status === 409) {
        try { sessionStorage.setItem('selkies_mode_flip', '1'); } catch (e) { /* ignore */ }
        safeSetItem(`${storageAppName}_stream_mode`, 'webrtc');
        console.warn('[websockets] Server is serving WebRTC (endpoint 409); switching stored mode.');
      }
    } catch (e) { /* unreachable server: plain reload below keeps retrying */ }
  }
  location.reload();
}

if (document.readyState === 'loading') {
  document.addEventListener('DOMContentLoaded', initWebsockets);
} else {
  initWebsockets();
}

/**
 * Closes every queued JPEG stripe image and resets the frame-boundary blit
 * latch, which stale would blit the previous mode's back-buffer once.
 */
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
  stripePendingFrameId = null;
  stripePendingDirty = false;
}

/** Closes every decoded stripe awaiting the paint loop. */
function clearDecodedStripesQueue() {
  while (decodedStripesQueue.length > 0) {
    const stripeData = decodedStripesQueue.shift();
    try {
      if (stripeData && stripeData.frame) stripeData.frame.close();
    } catch (e) {
      /* ignore */
    }
  }
  stripePendingFrameId = null;
  stripePendingDirty = false;
}

/**
 * Multistream Opus layouts for surround: the decoder needs an OpusHead
 * description carrying the same stream, coupled and mapping tables the
 * server encodes with.
 */
const MULTIOPUS_CLIENT_LAYOUTS = {
  6: { streams: 4, coupled: 2, mapping: [0, 4, 1, 2, 3, 5] },
  8: { streams: 5, coupled: 3, mapping: [0, 6, 1, 2, 3, 4, 5, 7] },
};

/**
 * The server's `audio_channels` setting, limited to the layouts the decoder handles.
 * @returns {number} 1, 2, 6 or 8; 2 when unset or unknown.
 */
function getAudioChannelCount() {
  const ch = parseInt(window.audio_channels, 10);
  return (ch === 1 || ch === 2 || ch === 6 || ch === 8) ? ch : 2;
}

/**
 * Builds the OpusHead description for a surround layout: magic, version 1,
 * channel count, a zero pre-skip (a live stream has nothing to trim), the
 * 48 kHz input rate, zero output gain, mapping family 1 (multistream), then
 * the stream and coupled counts and the channel mapping table.
 * @param {number} channels
 * @returns {ArrayBuffer|null} `null` for a layout the client does not know.
 */
function buildMultiopusDescription(channels) {
  const layout = MULTIOPUS_CLIENT_LAYOUTS[channels];
  if (!layout) return null;
  const buf = new ArrayBuffer(21 + channels);
  const u8 = new Uint8Array(buf);
  const dv = new DataView(buf);
  u8.set([0x4f, 0x70, 0x75, 0x73, 0x48, 0x65, 0x61, 0x64]);
  u8[8] = 1;
  u8[9] = channels;
  dv.setUint16(10, 0, true);
  dv.setUint32(12, 48000, true);
  dv.setInt16(16, 0, true);
  u8[18] = 1;
  u8[19] = layout.streams;
  u8[20] = layout.coupled;
  u8.set(layout.mapping, 21);
  return buf;
}

/**
 * Source of the Opus decode worker. It answers `init` (channels and the
 * surround description), `decode`, `reinitialize`, `updatePipelineStatus`
 * and `close`, and posts `decodedAudioData` with interleaved f32 PCM,
 * `decoderInitialized`, `decoderInitFailed` and `decoderError`; a fatal
 * decoder error is never re-initialized from inside, since a persistent
 * failure would spin, the page drives recovery.
 */
const audioDecoderWorkerCode = `
  let decoderAudio;
  let pipelineActive = true;
  let currentDecodeQueueSize = 0;
  // Set once the page hands over the worklet's line; until then decoded packets
  // go back through the page, which is also the path a worklet-less build takes.
  let pcmPort = null;
  let audioIn = null;
  let lastAudioTs = null;

  function audioTsNewer(a, b) {
    const d = (a - b) >>> 0;
    return d !== 0 && d < 0x80000000;
  }

  function extractOpusFrames(arrayBuffer) {
    const bytes = new Uint8Array(arrayBuffer);
    const nRed = bytes[1];
    if (!nRed) { lastAudioTs = null; return [arrayBuffer.slice(2)]; }
    // With n_red > 0 the bytes after the flag word are headers, not Opus, so a
    // truncated fixed part leaves no primary to salvage.
    if (arrayBuffer.byteLength < 6 + nRed * 4 + 1) { lastAudioTs = null; return []; }
    const pts = ((bytes[2] << 24) | (bytes[3] << 16) | (bytes[4] << 8) | bytes[5]) >>> 0;
    let pos = 6;
    const offsets = [], lens = [];
    for (let i = 0; i < nRed; i++) {
      const field = (bytes[pos + 1] << 16) | (bytes[pos + 2] << 8) | bytes[pos + 3];
      offsets.push((field >> 10) & 0x3fff);
      lens.push(field & 0x3ff);
      pos += 4;
    }
    pos += 1;
    // The declared block lengths must fit the payload: slice() clamps silently,
    // and the primary cannot be located without trustworthy lengths.
    let declared = pos;
    for (let i = 0; i < nRed; i++) { declared += lens[i]; }
    if (declared > arrayBuffer.byteLength) { lastAudioTs = null; return []; }
    const blocks = [];
    for (let i = 0; i < nRed; i++) {
      blocks.push({ ts: (pts - offsets[i]) >>> 0, buf: arrayBuffer.slice(pos, pos + lens[i]) });
      pos += lens[i];
    }
    blocks.push({ ts: pts, buf: arrayBuffer.slice(pos) });
    if (lastAudioTs === null) {
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
        // A fatal decoder error is not re-initialized from here: a persistent
        // failure would spin. The page drives recovery with its 'reinitialize'
        // message, which also re-checks the codec configuration.
        console.error('[AudioWorker] AudioDecoder error:', e.message, e);
        currentDecodeQueueSize = Math.max(0, currentDecodeQueueSize -1);
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
      if (pcmPort) pcmPort.postMessage({ audioData: pcmDataArrayBuffer }, [pcmDataArrayBuffer]);
      else self.postMessage({ type: 'decodedAudioData', pcmBuffer: pcmDataArrayBuffer }, [pcmDataArrayBuffer]);
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
      case 'pcmPort': pcmPort = event.data.port; break;
      case 'audioIn':
        audioIn = event.data.port;
        audioIn.onmessage = (m) => {
          if (!decoderAudio || decoderAudio.state !== 'configured') return;
          for (const opus of extractOpusFrames(m.data.buffer)) {
            if (!opus.byteLength) continue;
            try {
              decoderAudio.decode(new EncodedAudioChunk({
                type: 'key', timestamp: performance.now() * 1000, data: opus }));
            } catch (err) { /* a reconfiguring decoder drops the packet */ }
          }
        };
        break;
      case 'updatePipelineStatus': pipelineActive = data.isActive; break;
      case 'close':
        if (decoderAudio && decoderAudio.state !== 'closed') { try { decoderAudio.close(); } catch (e) { /* ignore */ } }
        decoderAudio = null; self.close(); break;
      default: break;
    }
  };
`;

/**
 * Source of the microphone AudioWorklet: converts captured frames to s16 and
 * posts them to the page, going quiet after a run of silent chunks.
 */
const micWorkletProcessorCode = `
class MicWorkletProcessor extends AudioWorkletProcessor {
  constructor() {
    super();
    this.SILENCE_THRESHOLD_CHUNKS = 300;
    this.silentChunkCounter = 0;
    this.isSending = true;
    // The encode worker's own line in: capture then reaches it whatever the
    // page's thread is doing. Until it is handed over, the page relays.
    this.out = this.port;
    this.port.onmessage = (e) => {
      if (e.data && e.data.port) this.out = e.data.port;
    };
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
        this.out.postMessage(int16Array.buffer, [int16Array.buffer]);
      }
    }
    return true;
  }
}
registerProcessor('mic-worklet-processor', MicWorkletProcessor);
`;

/**
 * Source of the microphone encode worker, which hosts the Opus AudioEncoder
 * off the main thread, mirroring the decode worker. The page forwards s16 PCM
 * as `pcm` messages and receives ready-to-send `0x02 + Opus` frames as
 * `chunk`; the restricted low-delay application is probed first.
 */
const micEncodeWorkerCode = `
  let encoder = null, tsUs = 0, active = true, wirePort = null;
  const onPcm = (buffer) => {
    if (!active || !encoder || encoder.state !== 'configured') return;
    if (!buffer || !(buffer instanceof ArrayBuffer) || buffer.byteLength === 0) return;
    const numFrames = buffer.byteLength / 2;
    const audioData = new AudioData({ format: 's16', sampleRate: 24000, numberOfFrames: numFrames, numberOfChannels: 1, timestamp: tsUs, data: buffer });
    tsUs += Math.round(numFrames * 1e6 / 24000);
    try { encoder.encode(audioData); } catch (err) {}
    audioData.close();
  };
  self.onmessage = async (e) => {
    const m = e.data;
    if (m.type === 'pcmPort') { m.port.onmessage = (ev) => onPcm(ev.data); return; }
    if (m.type === 'wirePort') { wirePort = m.port; return; }
    if (m.type === 'init') {
      const base = { codec: 'opus', sampleRate: 24000, numberOfChannels: 1, bitrate: 32000 };
      let cfg = { ...base, opus: { application: 'lowdelay' } };
      try { const s = await AudioEncoder.isConfigSupported(cfg); if (!s || !s.supported) cfg = base; } catch (err) { cfg = base; }
      try {
        encoder = new AudioEncoder({
          output: (chunk) => {
            if (!active) return;
            const buf = new ArrayBuffer(1 + chunk.byteLength);
            new Uint8Array(buf)[0] = 0x02;
            chunk.copyTo(new Uint8Array(buf, 1));
            if (wirePort) wirePort.postMessage(buf, [buf]);
            else self.postMessage({ type: 'chunk', buffer: buf }, [buf]);
          },
          error: (err) => self.postMessage({ type: 'error', message: String(err && err.message) }),
        });
        encoder.configure(cfg);
        self.postMessage({ type: 'ready' });
      } catch (err) { self.postMessage({ type: 'error', message: String(err && err.message) }); }
      return;
    }
    if (m.type === 'pcm') { onPcm(m.buffer); return; }
    if (m.type === 'stop') { active = false; try { encoder && encoder.state !== 'closed' && encoder.close(); } catch (err) {} encoder = null; return; }
  };
`;

/**
 * Starts the microphone uplink: getUserMedia at 24 kHz mono with processing
 * on, the capture worklet, and the encode worker whose Opus frames go
 * straight onto the socket, so only encoded bytes cross the wire and the
 * server decodes in pcmflux. Blocked for shared viewers.
 */
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
    const micEncodeWorkerURL = URL.createObjectURL(new Blob([micEncodeWorkerCode], { type: 'application/javascript' }));
    micEncodeWorker = new Worker(micEncodeWorkerURL);
    URL.revokeObjectURL(micEncodeWorkerURL);
    micEncodeWorker.onmessage = (event) => {
      const m = event.data;
      if (m.type === 'chunk') {
        if (!(websocket && websocket.readyState === WebSocket.OPEN && isMicrophoneActive)) return;
        try { websocket.send(m.buffer); } catch (e) { console.error("Error sending mic Opus:", e); }
      } else if (m.type === 'error') {
        console.error("Mic AudioEncoder error:", m.message);
      }
    };
    micEncodeWorker.onerror = (e) => console.error("Mic encode worker error:", e && e.message);
    micEncodeWorker.postMessage({ type: 'init' });
    if (websocket && websocket.connectSend) {
      // Capture worklet -> encode worker -> socket worker, no page hop: a
      // stalled page then delays outgoing voice no more than incoming.
      const pcmLine = new MessageChannel();
      micWorkletNode.port.postMessage({ port: pcmLine.port1 }, [pcmLine.port1]);
      micEncodeWorker.postMessage({ type: 'pcmPort', port: pcmLine.port2 }, [pcmLine.port2]);
      const wireLine = new MessageChannel();
      micEncodeWorker.postMessage({ type: 'wirePort', port: wireLine.port1 }, [wireLine.port1]);
      websocket.connectSend(wireLine.port2);
    }
    micWorkletNode.port.onmessage = (event) => {
      const pcm16Buffer = event.data;
      if (!(micEncodeWorker && isMicrophoneActive)) return;
      if (!pcm16Buffer || !(pcm16Buffer instanceof ArrayBuffer) || pcm16Buffer.byteLength === 0) return;
      try { micEncodeWorker.postMessage({ type: 'pcm', buffer: pcm16Buffer }, [pcm16Buffer]); }
      catch (e) { console.error("Mic PCM forward error:", e); }
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

/** Stops the microphone uplink and releases the stream, worklet, worker and context. */
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
  if (micEncodeWorker) {
    try { micEncodeWorker.postMessage({ type: 'stop' }); } catch (e) {}
    try { micEncodeWorker.terminate(); } catch (e) {}
    micEncodeWorker = null;
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

/**
 * Send-buffer budget of the webcam uplink, in milliseconds of its own bitrate.
 * A frame is dropped rather than queued past it: a camera frame that old is
 * of no use to the session by the time it lands, and a fixed byte budget is a
 * latency budget in disguise that at these rates spans many seconds.
 */
const WEBCAM_QUEUE_MS = 250;
/**
 * Payload of the last frame sent, so the budget above can never fall below one
 * frame. The rungs that are not rate-controlled (JPEG is quality-driven) put
 * frames far larger than a nominal bitrate implies, and a budget under one
 * frame admits nothing until each send has fully drained -- half the camera's
 * rate, or a stall on a slow link.
 */
let webcamLastFrameBytes = 0;

/**
 * Starts the webcam uplink (lib/webcam-capture.js): each encoded frame is
 * sent as one binary `[0x06][codec][flags][payload]` message that the
 * server's virtual camera decodes for the V4L2 device. Flags bit 0 marks a
 * keyframe; bits 1 to 2 carry the frame's clockwise rotation in quarter turns
 * and bit 3 a horizontal flip applied after it, the orientation metadata the
 * encoder never bakes into the bitstream. Frames are dropped rather than
 * queued while the socket is backed up (`WEBCAM_QUEUE_MS`). Blocked for
 * shared viewers.
 */
function startWebcamCapture() {
  if (isSharedMode || webcamCapture) {
    return;
  }
  webcamCapture = new WebcamCapture({
    encoderPreference: webcamEncoderPreference,
    sendFrame: (codec, keyframe, payload, rotation, flip) => {
      if (!(websocket && websocket.readyState === WebSocket.OPEN && isWebcamActive)) {
        return;
      }
      const messageBuffer = new ArrayBuffer(3 + payload.byteLength);
      const bytes = new Uint8Array(messageBuffer);
      bytes[0] = 0x06;
      bytes[1] = codec;
      bytes[2] = (keyframe ? 0x01 : 0x00) | ((((rotation || 0) / 90) & 0x03) << 1) | (flip ? 0x08 : 0x00);
      bytes.set(payload, 3);
      try {
        webcamLastFrameBytes = messageBuffer.byteLength;
        websocket.send(messageBuffer);
      } catch (e) {
        console.error("Error sending webcam frame:", e);
      }
    },
    canSend: () => !websocket
      || websocket.bufferedAmount <= Math.max(
        webcamCapture.bitrate / 8 * WEBCAM_QUEUE_MS / 1000, webcamLastFrameBytes),
    onStateChange: (active) => {
      isWebcamActive = active;
      postSidebarButtonUpdate();
    },
    onError: (error) => {
      console.error('Webcam capture error:', error);
      alert(`Webcam error: ${error.name || 'Error'} - ${error.message || error}`);
      stopWebcamCapture();
    },
  });
  if (websocket && websocket.connectWebcam) {
    // Encoded frames go encode worker -> socket worker directly; the socket
    // worker frames them and answers dropped-send gaps with a keyframe ask.
    webcamCapture.setWireProvider(() => {
      if (!websocket || !websocket.connectWebcam) return null;
      const line = new MessageChannel();
      websocket.connectWebcam(line.port2);
      return line.port1;
    });
  }
  webcamCapture.start(preferredWebcamDeviceId);
}

/** Stops the webcam uplink. */
function stopWebcamCapture() {
  if (webcamCapture) {
    webcamCapture.stop();
    webcamCapture = null;
  }
  if (isWebcamActive) {
    isWebcamActive = false;
    postSidebarButtonUpdate();
  }
}

/** Tears everything down on unload: timers, capture, socket, audio, decoders and buffers, then resets the UI state. */
function cleanup() {
  if (metricsIntervalId) {
    clearInterval(metricsIntervalId);
    metricsIntervalId = null;
  }
  if (backpressureIntervalId) {
    clearInterval(backpressureIntervalId);
    backpressureIntervalId = null;
  }
  clearSharedStallWatchdog();
  releaseWakeLock();
  if (window.isCleaningUp) return;
  window.isCleaningUp = true;
  console.log("Cleanup: Starting cleanup process...");
  if (!isSharedMode) { stopMicrophoneCapture(); stopWebcamCapture(); }

  if (websocket) {
    websocket.onopen = null;
    websocket.onmessage = null;
    websocket.onerror = null;
    websocket.onclose = null;
    if (websocket.readyState === WebSocket.OPEN || websocket.readyState === WebSocket.CONNECTING) websocket.close();
    websocket = null;
    window.selkiesTransport = null;
    videoDivertOn = false;
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

/**
 * Resets the video state after the server's PIPELINE_RESETTING: the shared
 * keyframe gate, the frame id, every buffer and the decoders of the current
 * mode, clearing the canvas for the modes that repaint it whole.
 * @param {string} [reason] Logged.
 */
function performServerInitiatedVideoReset(reason = "unknown") {
  console.log(`Performing server-initiated video reset. Reason: ${reason}. Current lastReceivedVideoFrameId before reset: ${lastReceivedVideoFrameId}`);

  lastReceivedVideoFrameId = -1;
  lastPresentedVideoFrameId = null;
  console.log(`  Reset lastReceivedVideoFrameId to ${lastReceivedVideoFrameId}.`);

  cleanupJpegStripeQueue();
  clearDecodedStripesQueue();

  if (currentEncoderMode === 'h264enc' || currentEncoderMode === 'h264enc-striped') {
    clearAllVncStripeDecoders();
  }

  if (canvasContext && canvas && !(currentEncoderMode === 'h264enc' || currentEncoderMode === 'h264enc-striped')) {
    try {
      canvasContext.setTransform(1, 0, 0, 1, 0, 0);
      canvasContext.clearRect(0, 0, canvas.width, canvas.height);
      console.log("  Cleared canvas during server-initiated reset.");
    } catch (e) {
      console.error("  Error clearing canvas during server-initiated reset:", e);
    }
  }

}

let lastKeyframeRequestTime = 0;
/**
 * Asks the server for an IDR when a decoder waits for its first keyframe
 * (a recreated stripe decoder, a shared viewer joining). The GOP is
 * infinite, so this is the only recovery path and shared viewers request
 * too; debounced here, harder for shared viewers, and rate-limited server-side.
 */
function requestKeyframe() {
    const now = performance.now();
    if (now - lastKeyframeRequestTime < (isSharedMode ? 1500 : 500)) return;
    lastKeyframeRequestTime = now;
    if (websocket && websocket.readyState === WebSocket.OPEN) {
        websocket.send("REQUEST_KEYFRAME");
    }
}

/**
 * Rebuilds every video decoder so a changed acceleration preference takes
 * hold, then resyncs from a fresh IDR. The main decoder is only fed in shared
 * mode; the stripe and worker decoders are rebuilt from the next keyframe by
 * the paths that own them, and a worker decoder disqualified by the same
 * broken path gets its turn back.
 */
function restartDecodersForAcceleration() {
    workerDecoderCodec = null; workerDecoderW = 0; workerDecoderH = 0;
    workerKeyframeCodec = null;
    workerDecodeFailed = false;
    if (videoWorker) {
        try { videoWorker.postMessage({ type: 'closeDecoder' }); } catch (_) {}
    }
    clearAllVncStripeDecoders();
    lastKeyframeRequestTime = 0;
    requestKeyframe();
}

/**
 * The decoder fallback ladder. A codec reclaimed by the browser is a soft
 * error left to the tab-focus re-init. A decoder that accepted its config and
 * then failed is the signature of a broken hardware path, so the first hard
 * error retries the same encoder on software decode; errors from the decoders
 * that switch replaced are absorbed for a settle period. A failure after that
 * forgets the preference, counts a crash, and reloads: a shared viewer just
 * resyncs, a controller resets its settings to safe defaults, stepping the
 * encoder down to h264enc and, at three crashes, to jpeg. jpeg mode runs no
 * VideoDecoder, so an error there is handover noise from a stream the server
 * has yet to stop and never escalates.
 * @param {Error|DOMException} error
 * @param {string} context Which decoder failed.
 */
function initiateFallback(error, context) {
    if (error.name === 'QuotaExceededError' || (error.message && error.message.includes('reclaimed'))) {
        console.warn(`[initiateFallback] Ignoring soft error (Context: ${context}): Codec reclaimed by browser. Waiting for tab focus to re-initialize.`);
        return;
    }
    if (!softwareDecodeAttempted && !window.isFallingBack &&
        currentEncoderMode !== 'jpeg') {
        softwareDecodeAttempted = true;
        softwareDecodeSwitchedAt = performance.now();
        console.warn(`[initiateFallback] Decoder error (Context: ${context}); retrying on software decode.`, error);
        rememberSoftwareDecode(true);
        restartDecodersForAcceleration();
        return;
    }
    if (performance.now() - softwareDecodeSwitchedAt < SOFTWARE_DECODE_SETTLE_MS) {
        console.warn(`[initiateFallback] Ignoring decoder error (Context: ${context}) from the decoders the software switch replaced.`);
        return;
    }
    console.error(`FATAL DECODER ERROR (Context: ${context}).`, error);
    if (window.isFallingBack) return;
    window.isFallingBack = true;
    rememberSoftwareDecode(false);
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
        let crashCount = parseInt(window.localStorage.getItem(CRASH_COUNT_KEY) || '0');
        crashCount++;
        safeSetItem(CRASH_COUNT_KEY, crashCount.toString());
        if (crashCount >= 3) {
            setStringParam('encoder', 'jpeg');
            safeSetItem(CRASH_COUNT_KEY, '0');
        } else if (currentEncoderMode !== 'jpeg') {
            setStringParam('encoder', 'h264enc');
        } else {
            // Un-escalating from jpeg would loop the ladder on builds whose
            // WebCodecs claims H.264 support but fails at decode().
            safeSetItem(CRASH_COUNT_KEY, '0');
        }
        setBoolParam('video_fullcolor', false);
        setIntParam('framerate', 60);
        setIntParam('video_crf', 25);
        setBoolParam('manual_resolution', false);
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

/**
 * Builds the UI and checks the engine: a secure context is required; without
 * WebCodecs the stream is pinned to the jpeg encoder, which decodes through
 * createImageBitmap, and a server-locked H.264 encoder is reported when it
 * arrives rather than decoded into a crash loop.
 * @returns {boolean} False when the page cannot run.
 */
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
        console.warn("VideoDecoder API unavailable: the stream is pinned to the jpeg encoder.");
        pinJpegEncoder();
    } else {
        console.log("Pre-flight checks passed: Secure context and VideoDecoder API are available.");
    }
    return true;
}

/** Pins the jpeg encoder, the fallback ladder's last rung. */
function pinJpegEncoder() {
    currentEncoderMode = 'jpeg';
    setStringParam('encoder', 'jpeg');
}

let undecodableEncoderNoticeShown = false;
/**
 * Reports a server-locked encoder this engine cannot decode instead of showing nothing.
 * @param {string} encoderName
 */
function showUndecodableEncoderNotice(encoderName) {
    console.error(`Encoder ${encoderName} needs the WebCodecs API, which this browser lacks.`);
    if (statusDisplayElement) {
        statusDisplayElement.textContent = 'Error: The session streams an encoder this browser cannot decode without the WebCodecs API.';
        statusDisplayElement.classList.remove('hidden');
    }
    undecodableEncoderNoticeShown = true;
}

/** Hides the undecodable-encoder notice once a decodable encoder is in use. */
function clearUndecodableEncoderNotice() {
    if (!undecodableEncoderNoticeShown) return;
    undecodableEncoderNoticeShown = false;
    if (statusDisplayElement) statusDisplayElement.classList.add('hidden');
}

window.addEventListener('beforeunload', cleanup);
window.webrtcInput = null;
}
