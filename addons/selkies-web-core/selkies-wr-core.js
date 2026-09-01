/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 *
 * This file incorporates work covered by the following copyright and
 * permission notice:
 *
 *   Copyright 2019 Google LLC
 *
 *   Licensed under the Apache License, Version 2.0 (the "License");
 *   you may not use this file except in compliance with the License.
 *   You may obtain a copy of the License at
 *
 *        http://www.apache.org/licenses/LICENSE-2.0
 *
 *   Unless required by applicable law or agreed to in writing, software
 *   distributed under the License is distributed on an "AS IS" BASIS,
 *   WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 *   See the License for the specific language governing permissions and
 *   limitations under the License.
 */

/**
 * The WebRTC streaming core: one bundled peer connection carrying the video
 * and audio the server encodes, the webcam and microphone uplinks on sendonly
 * transceivers, and a data channel for everything else.
 *
 * Client to server on the data channel, as text: `SETTINGS,{json}` (the
 * persisted settings on connect and the passthrough settings later), `r,WxH`
 * (stream resolution), `s,DPI`, `vb,kbps`, `ab,bps`, `_arg_fps,N`, `_crf,N`,
 * `_rc,mode`, `_ebc,bool`, `cmd,command`, `kr` (release every key), `cr`
 * (cache-only clipboard fetch), `REQUEST_CLIPBOARD`, `START_VIDEO` /
 * `STOP_VIDEO` (this peer's feed), `SET_NATIVE_CURSOR_RENDERING,0|1`, the
 * `_f,fps` and `_l,ms` client metrics, `_stats_video,{json}` when the server
 * asked for raw reports, and the chunked clipboard transfer of
 * `lib/clipboard-worker-bridge.js`. Server to client arrives through the
 * `WebRTCClient` callbacks: the settings payload, `clipboard-msg*` messages,
 * cursor and display-config updates, stats, and system actions (`reload`,
 * `mk_access,0|1`, `command_error,text`, `auth_success,{json}` /
 * `role_update,{json}`, `resolution,WxH`).
 *
 * The page hash selects the role: none is the controller, `#shared` a strict
 * viewer, `#playerN` a viewer with gamepad slot N, and `#display2-<position>`
 * the secondary display page, which streams its own region of the extended
 * desktop and keeps its per-display settings under `_display2` keys.
 * Signaling scopes controller and slot uniqueness per display id, the server
 * runs one pipeline per display, and the position rides the connect metadata.
 *
 * Contract with the dashboards. Globals published on `window`: `selkiesLogs`
 * (capped log ring buffers), `fps`, `network_stats`, `gpu_stats`,
 * `system_stats`, `currentAudioBufferSize`, `manualResolution`,
 * `enable_resize`, `streamResolutionDiverged`, `webrtcInput`, and every server
 * setting as `window[key]`. Window messages handled (same origin):
 * `setScaleLocally`, `resetResolutionToWindow`, `setManualResolution`,
 * `setUseCssScaling`, `settings`, `command`, `pipelineControl`,
 * `gamepadControl`, `clipboardUpdateFromUI`, `clipboardImageUpdate`,
 * `audioDeviceSelected`, `requestFullscreen`, `setSynth`,
 * `showVirtualKeyboard`, `setAntiAliasing`, `setUseBrowserCursors`,
 * `touchinput:trackpad`, `touchinput:touch`, plus the `requestFileUpload` DOM
 * event. Window messages posted: `sidebarButtonStatusUpdate`,
 * `pipelineStatusUpdate`, `effectiveCursorState`, `serverSettings`,
 * `clipboardContentUpdate`, `fileUpload` warnings, `trackpadModeUpdate`,
 * `clientRoleUpdate`, `toggleDashboard`, `toggleTouchGamepad`. Flags read:
 * `window.__selkiesModeSwitching` (a mode switch in progress suppresses
 * alerts and recovery reloads), `window.__selkiesAuthProbe` (re-presents the
 * login after an auth drop), `window.clipboard_enabled`.
 * @module
 */

import { WebRTCClient } from "./lib/webrtc";
import { WebRTCSignaling } from "./lib/signaling";
import { Input } from "./lib/input";
import { createClipboardSync, createClipboardGestures, createDeferredClipboardWriter, createLocalClipboardSender, createMultipartClipboardState, createTaggedClipboardFetch, clipboardPreviewMessage, reencodeBlobAsPng, localClipboardBlocker, writeImageToLocalClipboard, digestedPayload } from "./lib/clipboard-sync.js";
import { createFileUploader } from "./lib/file-upload.js";
import { ClipboardWorkerBridge, sendClipboardChunked } from './lib/clipboard-worker-bridge.js'
import { detectKeyboardLayout } from './lib/keyboard-layout.js';
import { installAuthGuard } from './lib/auth-guard.js';
import { installSessionCookie, sessionAuthHeaders } from './lib/session-token.js';
import { storageKeyForServerKey } from './lib/conditional-settings.js';
import { getRoutePrefix, getStorageAppName, canDecodeFullColor } from './lib/util.js';

installAuthGuard();
installSessionCookie();

/**
 * Local keyboard layout hint, resolved once at script init so it is ready by
 * the time signaling and ICE bring the data channel up; `null` is unknown and
 * omitted. This core sends its settings once per session, so a probe that
 * lands after that send follows on its own.
 */
let detectedKeyboardLayout = null;
let persistentSettingsSent = false;
/** The live WebRTCClient, mirrored here for module-scope consumers. */
let activeWebrtcClient = null;
detectKeyboardLayout().then((layout) => {
    detectedKeyboardLayout = layout;
    if (layout && persistentSettingsSent && activeWebrtcClient) {
        try {
            activeWebrtcClient.sendDataChannelMessage(`SETTINGS,${JSON.stringify({ keyboardLayout: layout })}`);
        } catch (e) { /* session may not be up yet */ }
    }
});

/** Per-transfer id, so concurrent multipart clipboard sends never interleave. */
let __clipboardTransferCounter = 0;
/** The server's `command_enabled`; true until a server advertises otherwise. */
let serverCommandEnabled = true;

/** Injects the stylesheet for the video container, overlay and status bar. */
function InitUI() {
	let style = document.createElement('style');
	style.textContent = `
	body {
		background-color: #000000;
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
	.video-container #overlayInput{
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

	.loading-text {
		margin-top: 1em;
	}

	.hidden {
		display: none !important;
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
	`;
  document.head.appendChild(style);
}

/**
 * Builds the WebRTC core.
 * @returns {{initialize: () => void, cleanup: () => void}} `initialize`
 *     builds the DOM, connects signaling and opens the peer connection;
 *     `cleanup` tears the session down and resets every session-scoped value.
 */
export default function webrtc() {
	let appName;
	let crf = 23;
	/** Video bitrate in kbps. */
	let videoBitRate = 8000;
	let videoFramerate = 60;
	/** Audio bitrate in bps. */
	let audioBitRate = 128000;
	let showStart = false;
	let showDrawer = false;
	/**
	 * Gaming mode is fullscreen holding the pointer and the keyboard; a transport
	 * switch rebuilds Input, so the mode it was in is carried over.
	 */
	let gamingModeActive = false;
	/** Cap of the `window.selkiesLogs` ring buffers; every entry also goes to the console. */
	const MAX_LOG_ENTRIES = 1000;
	const pushCapped = (arr, v) => { arr.push(v); if (arr.length > MAX_LOG_ENTRIES) arr.shift(); };
	let logEntries = [];
	let debugEntries = [];
	window.selkiesLogs = { log: logEntries, debug: debugEntries };
	let status = 'connecting';
	let clipboardStatus = 'disabled';
	/** Server-synced direction gates: in is client to server, out is server to client. */
	let clipboard_in_enabled = true;
	let clipboard_out_enabled = true;
	let windowResolution = [];
	let encoderLabel = "";
	let encoder = "";
	let rateControlMode = "cbr";
	let gamepad = {
			gamepadState: 'disconnected',
			gamepadName: 'none',
	};

	let connectionStat = {
		connectionStatType: "unknown",
		connectionLatency: 0,
		connectionVideoLatency: 0,
		connectionAudioLatency: 0,
		connectionAudioCodecName: "NA",
		connectionAudioBitrate: 0,
		connectionPacketsReceived: 0,
		connectionPacketsLost: 0,
		connectionBytesReceived: 0,
		connectionBytesSent: 0,
		connectionCodec: "unknown",
		connectionVideoDecoder: "unknown",
		connectionResolution: "",
		connectionFrameRate: 0,
		connectionVideoBitrate: 0,
		connectionAvailableBandwidth: 0
	};

	var videoElement = null;
	var audioElement = null;
	/**
	 * Set on a fatal server verdict (close 4000/4001): blocks every recovery
	 * reload, the peer connection's and the resume watchdog's, so a superseded
	 * page cannot re-enter the takeover loop.
	 */
	let fatalConnectionHalt = false;
	/**
	 * Last stream resolution asked of the server, in physical pixels; compared
	 * with the track's intrinsic size to detect a realized size that differs
	 * from the request (snapping, a rejected resize).
	 */
	var lastRequestedStreamRes = null;
	/** Screen Wake Lock sentinel, null while not held. */
	let wakeLockSentinel = null;
	let preferredOutputDeviceId = null;
	let preferredInputDeviceId = null;
	let serverLatency = 0;
	let resizeRemote = false;
	let scaleLocal = false;
	let debug = false;
	let turnSwitch = false;
	let playButtonElement = null;
	let statusDisplayElement = null;
	let rtime = null;
	/** Resize debounce delay in milliseconds. */
	let rdelta = 500;
	let rtimeout = false;
	let manualWidth, manualHeight = 0;
	window.manualResolution = false;
	window.fps = 0;
	window.currentAudioBufferSize = 0;
	let enableWebrtcStatics = false;

	var videoConnected = "";
	var audioConnected = "";
	var statWatchEnabled = false;
	var webrtc = null;
	var input = null;
	/** Interval ids, cleared on cleanup so a reconnect never double-starts a loop. */
	let statsLoopId = null;
	let metricsLoopId = null;
	/**
	 * CSS scaling on means dpr 1 everywhere; off, the resolution senders and the
	 * input math apply devicePixelRatio. Off by default so an auto-resolution
	 * HiDPI client renders a physical-resolution buffer.
	 */
	let useCssScaling = false;
	/**
	 * The desktop DPI slider value (96 is 100%), independent of the resolution
	 * and the HiDPI toggle; derived from devicePixelRatio unless a pick is stored.
	 */
	let scalingDPI = 96;
	let isVideoPipelineActive = true;
	let isAudioPipelineActive = true;
	let isMicrophoneActive = false;
	let isWebcamActive = false;
	let webcamBusy = false;
	let preferredWebcamDeviceId = null;
	let isGamepadEnabled = true;

	/**
	 * Per-message budget on the data channel: the negotiated SCTP maximum
	 * message size (the minimum of both ends) where the browser exposes it,
	 * else the 256 KiB pre-negotiation standard, capped at 1 MiB to bound
	 * per-message buffering, less 512 bytes for the message prefix.
	 * @returns {number}
	 */
	const dcMessageBudget = () => {
		const nego = (typeof webrtc !== 'undefined' && webrtc && webrtc.peerConnection &&
			webrtc.peerConnection.sctp && webrtc.peerConnection.sctp.maxMessageSize) || 0;
		const limit = nego > 0 ? Math.min(nego, 1024 * 1024) : 256 * 1024;
		return limit - 512;
	};
	/**
	 * Raw bytes per clipboard chunk, before base64 expansion, and the channel
	 * backlog a transfer waits below before queueing the next.
	 *
	 * Sized for latency, not capacity: input shares this ordered channel and
	 * the media streams share its bandwidth, so a transfer left to fill the
	 * channel puts both behind it. A multiple of 3, to concatenate as base64.
	 */
	const CLIPBOARD_CHUNK_SIZE = 16383;
	const CLIPBOARD_BACKLOG_BYTES = 64 * 1024;
	const CLIENT_CONTROLLER = "controller";
	const CLIENT_VIEWER = "viewer";

	let detectedSharedModeType = null;
	let playerInputTargetIndex = 0;
	let clientRole = null;
	let clientSlot = null;

	/** Render and input preferences, persisted under the keys the WebSocket core uses too. */
	let antiAliasingEnabled = true;
	let trackpadMode = false;
	/**
	 * Cursor-rendering preference in force: seeded from localStorage at
	 * connect, then updated by a dashboard pick (persisted) or a server-pushed
	 * value (not persisted, so a later server-side change stays re-pushable).
	 * The effective value adds the multi-monitor override.
	 */
	let useBrowserCursors = true;
	/**
	 * Whether a secondary display page is connected (server display-config
	 * broadcast). Multi-monitor forces browser-cursor rendering: the
	 * server-drawn cursor overlay tracks only one capture region.
	 */
	let isSecondaryDisplayConnected = false;
	/** Rounds resolutions to multiples of 16 (encoder macroblocks) instead of 2. */
	let force_aligned_resolution = false;

	let enable_binary_clipboard = true;
	let clipboardWorker = new ClipboardWorkerBridge();
	/** Multipart download state and connect-time cache-only fetch tracking (`lib/clipboard-sync.js`). */
	const multipartClipboard = createMultipartClipboardState(
		(mime) => clipboardWorker.decodeStream(mime));
	const taggedClipboardFetch = createTaggedClipboardFetch();
	const armTaggedClipboardReply = () => taggedClipboardFetch.arm();
	const consumeInitClipboardFetch = () => taggedClipboardFetch.consume();
	/** Server-clipboard cache, change-only sync and copy request queue; the send hook late-binds `webrtc`. */
	/**
	 * PNG-normalizes an image on the worker, which is where the decode and
	 * re-encode of a large one belong; the page's own canvas covers a worker
	 * that cannot do it.
	 */
	const reencodePngOffThread = (blob) => clipboardWorker.reencodePng(blob)
		.then((r) => r.result)
		.catch(() => reencodeBlobAsPng(blob));
	const clipboardSync = createClipboardSync({
		sendRequest: () => webrtc.sendDataChannelMessage('REQUEST_CLIPBOARD'),
		digestBytes: async (buf) => {
			const { byteLength, hash } = await clipboardWorker.hashBytes(buf);
			return digestedPayload(byteLength, hash);
		}
	});
	/**
	 * Retry queue for local clipboard writes of server pushes, which carry no
	 * user activation: Firefox and WebKit reject the write until the next real
	 * gesture.
	 */
	const deferredClipboardWriter = createDeferredClipboardWriter();
	/**
	 * Chromium-engine detection: userAgentData brands are authoritative and
	 * `window.chrome` a fallback for older engines that expose no brands; iOS,
	 * Firefox and CriOS are excluded.
	 */
	const isChromium = (() => {
		const isIOS = /iPad|iPhone|iPod/.test(navigator.userAgent) ||
			(navigator.platform === 'MacIntel' && navigator.maxTouchPoints > 1);
		const isFirefox = /Firefox|FxiOS/.test(navigator.userAgent);
		const isCriOS = /CriOS/.test(navigator.userAgent);
		const brands = (navigator.userAgentData && navigator.userAgentData.brands) || [];
		const isChromiumBrand = brands.some((b) => /Chromium|Google Chrome/.test(b.brand));
		return (isChromiumBrand || typeof window.chrome !== 'undefined') && !isIOS && !isFirefox && !isCriOS;
	})();

	const hash = window.location.hash;
	if (hash === '#shared') {
        clientRole = CLIENT_VIEWER;
        clientSlot = -1;
        detectedSharedModeType = 'shared';
        playerInputTargetIndex = undefined;
    } else if (hash.startsWith('#player')) {
        clientRole = CLIENT_VIEWER;
        const playerNum = parseInt(hash.substring(7), 10);
        clientSlot = playerNum || null;
        if (playerNum >= 2 && playerNum <= 4) {
            detectedSharedModeType = `player${playerNum}`;
            playerInputTargetIndex = playerNum - 1;
        }
    } else {
        clientRole = CLIENT_CONTROLLER;
        clientSlot = 1;
        playerInputTargetIndex = 0;
    }

	const isSharedMode = detectedSharedModeType !== null;
	const isStrictViewer = detectedSharedModeType === "shared";
	/** Whether the server's `mk_access` action granted this viewer the full input context. */
	let collabInputGranted = false;

	const storageAppName = getStorageAppName();
	/** Writes a key, degrading a full or unavailable store to a warning. */
	const safeSetItem = (key, value) => {
		try {
			window.localStorage.setItem(key, value);
		} catch (e) {
			console.warn(`Selkies: could not persist '${key}' to localStorage:`, e);
		}
	};

	/**
	 * Settings that get a `_display2` suffix on the second-display page, so the
	 * two displays' picks never share a key; must match the dashboards'
	 * `getPrefixedKey` and the WebSocket core.
	 */
	const storageDisplayId = window.location.hash.startsWith('#display2') ? 'display2' : 'primary';
	const PER_DISPLAY_SETTINGS = [
		'framerate', 'video_crf', 'video_fullcolor',
		'video_streaming_mode', 'use_cpu',
		'video_paintover_crf', 'video_paintover_burst_frames', 'use_paint_over_quality',
		'manual_resolution', 'manual_width', 'manual_height',
		'encoder', 'scaleLocallyManual', 'use_browser_cursors', 'rate_control_mode',
		'video_bitrate', 'force_aligned_resolution'
	];
	const storageKeyFor = (key) => {
		const prefixedKey = `${storageAppName}_${key}`;
		if (storageDisplayId === 'display2' && PER_DISPLAY_SETTINGS.includes(key)) {
			return `${prefixedKey}_${storageDisplayId}`;
		}
		return prefixedKey;
	};

	const getIntParam = (key, default_value) => {
		const prefixedKey = storageKeyFor(key);
		const value = window.localStorage.getItem(prefixedKey);
		return (value === null || value === undefined) ? default_value : parseInt(value);
	};
	/** Like getIntParam but keeps fractions: range-bounded settings can be floats. */
	const getFloatParam = (key, default_value) => {
		const prefixedKey = storageKeyFor(key);
		const value = window.localStorage.getItem(prefixedKey);
		const parsed = parseFloat(value);
		return (value === null || value === undefined || isNaN(parsed)) ? default_value : parsed;
	};
	const setIntParam = (key, value) => {
		const prefixedKey = storageKeyFor(key);
		if (value === null || value === undefined) {
				window.localStorage.removeItem(prefixedKey);
		} else {
				safeSetItem(prefixedKey, value.toString());
		}
	};
	const getBoolParam = (key, default_value) => {
		const prefixedKey = storageKeyFor(key);
		const v = window.localStorage.getItem(prefixedKey);
		if (v === null) {
				return default_value;
		}
		return v.toString().toLowerCase() === 'true';
	};
	const setBoolParam = (key, value) => {
		const prefixedKey = storageKeyFor(key);
		if (value === null || value === undefined) {
				window.localStorage.removeItem(prefixedKey);
		} else {
				safeSetItem(prefixedKey, value.toString());
		}
	};
	const getStringParam = (key, default_value) => {
		const prefixedKey = storageKeyFor(key);
		const value = window.localStorage.getItem(prefixedKey);
		return (value === null || value === undefined) ? default_value : value;
	};
	const setStringParam = (key, value) => {
		const prefixedKey = storageKeyFor(key);
		if (value === null || value === undefined) {
				window.localStorage.removeItem(prefixedKey);
		} else {
				safeSetItem(prefixedKey, value.toString());
		}
	};

	/** Prefixes a log line with the wall-clock time. */
	var applyTimestamp = (msg) => {
		var now = new Date();
		var ts = now.getHours() + ":" + now.getMinutes() + ":" + now.getSeconds();
		return "[" + ts + "]" + " " + msg;
	}

	/** Rounds a dimension down to the encoder alignment: 2 (YUV 4:2:0 chroma), or 16 when forced. */
	const alignResolution = (num) => {
		const alignment = force_aligned_resolution ? 16 : 2;
		return Math.floor(num / alignment) * alignment;
	};

	/**
	 * Applies the effective cursor-rendering setting: the user preference, or
	 * browser cursors forced on when this page is a secondary display or the
	 * primary while a secondary is connected, since the server-drawn overlay
	 * tracks only one capture region. The dashboard is told the value in
	 * force, so its toggle reflects the override.
	 */
	function applyEffectiveCursorSetting() {
		const userPreference = useBrowserCursors;
		const isDisplay2 = window.location.hash.startsWith('#display2');
		const isMultiMonitorActive = (isDisplay2 || isSecondaryDisplayConnected);
		const finalSetting = isMultiMonitorActive ? true : userPreference;
		if (input && typeof input.setUseBrowserCursors === 'function') {
			console.log(`Applying effective cursor setting. Multi-monitor: ${isMultiMonitorActive}, User Pref: ${userPreference}, Final: ${finalSetting}`);
			input.setUseBrowserCursors(finalSetting);
		}
		try {
			window.postMessage({ type: 'effectiveCursorState', value: finalSetting }, window.location.origin);
		} catch (e) { /* postMessage unavailable */ }
	}

	/** Starts playback after the user's gesture and takes the wake lock. */
	function playStream() {
		showStart = false;
		if (playButtonElement) playButtonElement.classList.add('hidden');
		webrtc.playStream();
		requestWakeLock();
	}

	/** Keeps the screen awake while streaming; a no-op when held or where the API is absent. */
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

	/** Releases the wake lock if held. */
	const releaseWakeLock = async () => {
		if (wakeLockSentinel !== null) {
			await wakeLockSentinel.release();
			wakeLockSentinel = null;
		}
	};

	let hiddenVideoPauseTimer = null;
	let videoPausedForHiddenTab = false;
	let resumeWatchdogTimer = null;
	let resumeWatchdogAttempts = 0;
	const RESUME_WATCHDOG_MS = 3000;
	const RESUME_WATCHDOG_MAX_ATTEMPTS = 3;

	/** Cancels the resume watchdog. */
	function clearResumeWatchdog() {
		if (resumeWatchdogTimer !== null) {
			clearTimeout(resumeWatchdogTimer);
			resumeWatchdogTimer = null;
		}
		resumeWatchdogAttempts = 0;
	}

	/**
	 * Verifies that frames follow a START_VIDEO. The resume is one message on
	 * a data channel that can close at that very moment, and a lost one is
	 * answered with nothing: the peer would stay subscribed to a feed nobody
	 * encodes. The element's playback clock is the signal WebRTC has, so the
	 * check is whether it advanced past the mark taken here.
	 */
	function armResumeWatchdog() {
		if (resumeWatchdogTimer !== null) clearTimeout(resumeWatchdogTimer);
		const mark = videoElement ? videoElement.currentTime : 0;
		resumeWatchdogTimer = setTimeout(() => checkResumed(mark), RESUME_WATCHDOG_MS);
	}

	/**
	 * Watchdog tick: resends START_VIDEO while the playback clock has not moved
	 * past `mark`, up to RESUME_WATCHDOG_MAX_ATTEMPTS, then reloads to
	 * reconnect unless a fatal verdict or a mode switch forbids it. A tab
	 * hidden again stands the watchdog down: the visibility path owns that
	 * state. Each attempt also replays the element, since one the browser
	 * paused while the tab was away plays nothing however much RTP arrives.
	 * @param {number} mark Playback time when the watchdog was armed.
	 */
	function checkResumed(mark) {
		resumeWatchdogTimer = null;
		if (document.hidden || !webrtc) { resumeWatchdogAttempts = 0; return; }
		if (videoElement && videoElement.currentTime > mark) {
			resumeWatchdogAttempts = 0;
			return;
		}
		if (videoElement && videoElement.paused) videoElement.play().catch(() => {});
		resumeWatchdogAttempts++;
		if (resumeWatchdogAttempts <= RESUME_WATCHDOG_MAX_ATTEMPTS) {
			console.warn(`No video after resuming; resend attempt ${resumeWatchdogAttempts}/${RESUME_WATCHDOG_MAX_ATTEMPTS}.`);
			try { webrtc.sendDataChannelMessage('START_VIDEO'); } catch (_) {}
			armResumeWatchdog();
			return;
		}
		resumeWatchdogAttempts = 0;
		if (fatalConnectionHalt) return;
		if (typeof window !== 'undefined' && window.__selkiesModeSwitching) return;
		console.warn('[webrtc] no video after resuming; reloading to reconnect.');
		location.reload();
	}

	/**
	 * Pauses this peer's video feed while the tab is hidden and resumes it on
	 * show, re-acquiring the wake lock the browser dropped.
	 *
	 * A hidden tab's rendering is throttled anyway, so its encode and bandwidth
	 * are waste. STOP_VIDEO and START_VIDEO gate only this peer's RTP sender,
	 * and the shared capture stops once every consumer is paused, so viewer
	 * pages send them too. The pause is deferred because a navigating document
	 * reports hidden just before it unloads and timers never fire in an
	 * unloading document, so only a genuine tab-hide sends it. Recovery on
	 * resume is the server's IDR (plus PLI); the client sends no keyframe
	 * requests, and the resume watchdog verifies frames follow.
	 */
	async function handleVisibilityChange() {
		if (document.hidden) {
			clearResumeWatchdog();
			if (hiddenVideoPauseTimer === null) {
				hiddenVideoPauseTimer = setTimeout(() => {
					hiddenVideoPauseTimer = null;
					if (!document.hidden || videoPausedForHiddenTab || !webrtc) return;
					// A feed the user stopped from the dashboard must not be resurrected on show.
					if (!isVideoPipelineActive) return;
					videoPausedForHiddenTab = true;
					try { webrtc.sendDataChannelMessage('STOP_VIDEO'); } catch (_) {}
					console.log("Tab hidden: sent STOP_VIDEO to pause this peer's feed.");
				}, 250);
			}
			return;
		}
		if (hiddenVideoPauseTimer !== null) {
			clearTimeout(hiddenVideoPauseTimer);
			hiddenVideoPauseTimer = null;
		}
		if (videoPausedForHiddenTab) {
			videoPausedForHiddenTab = false;
			if (webrtc) {
				try { webrtc.sendDataChannelMessage('START_VIDEO'); } catch (_) {}
			}
			console.log("Tab visible: sent START_VIDEO to resume this peer's feed.");
			armResumeWatchdog();
		} else if (videoElement && videoElement.paused) {
			// The browser can have paused the element itself while the tab was away.
			videoElement.play().catch(() => {});
		}
		if (wakeLockSentinel === null) {
			await requestWakeLock();
		}
	}

	/**
	 * Routes audio to the preferred output device: the `video` element carries
	 * the bundled audio track, so `setSinkId` on it moves the audio sink.
	 */
	async function applyOutputDevice() {
		if (!preferredOutputDeviceId || !videoElement) return;
		if (!('setSinkId' in HTMLMediaElement.prototype) || typeof videoElement.setSinkId !== 'function') {
			console.warn('setSinkId not supported; cannot select audio output device.');
			return;
		}
		try {
			await videoElement.setSinkId(preferredOutputDeviceId);
			console.log(`Playback output set to device: ${preferredOutputDeviceId}`);
		} catch (err) {
			console.error(`Failed to set audio output device: ${err.name}, ${err.message}`);
		}
	}

	/**
	 * Shows the sentence-cased status (the internal value stays lower-case for
	 * comparisons); once connected, hides it and shows the play button if
	 * playback still needs a gesture.
	 */
	function updateStatusDisplay() {
		if (statusDisplayElement) {
			statusDisplayElement.textContent = status ? status.charAt(0).toUpperCase() + status.slice(1) : status;
			if (status == 'connected') {
				statusDisplayElement.classList.add("hidden");
				if (playButtonElement && showStart) {
					playButtonElement.classList.remove('hidden');
				}
			}
		}
	}

	/**
	 * Picks the video's `image-rendering`: pixelated with anti-aliasing off or
	 * at 1:1, `auto` (smoothed) when CSS-scaled above 1 dpr.
	 */
	function updateVideoImageRendering(){
		if (!videoElement) return;

		if (!antiAliasingEnabled) {
			if (videoElement.style.imageRendering !== 'pixelated') {
				videoElement.style.imageRendering = 'pixelated';
			}
			return;
		}
		const dpr = window.devicePixelRatio || 1;
		const isOneToOne = !useCssScaling || (useCssScaling && dpr <= 1);
		if (isOneToOne) {
			if (videoElement.style.imageRendering !== 'pixelated') {
				console.log("Setting video rendering to 'pixelated' for sharp display.");
				videoElement.style.imageRendering = 'pixelated';
			}
		} else {
			if (videoElement.style.imageRendering !== 'auto') {
				console.log("Setting video rendering to 'auto' for smooth upscaling.");
				videoElement.style.imageRendering = 'auto';
			}
		}
	};

	/**
	 * Applies the server's settings payload to the runtime and reconciles the
	 * user's stored overrides against it.
	 *
	 * Every value is applied to `window[key]`; only a genuine user override is
	 * persisted. A server value with no stored override is not written to
	 * localStorage, so a later server-side change can still be re-pushed, and a
	 * locked value is never written into the user's key, where it would
	 * masquerade as their pick after an unlock. The override is read under the
	 * key the dashboard writes (HiDPI stores as `useCssScaling`), or an
	 * unlocked operator value would win forever. An unlocked operator override
	 * with no stored pick is reported back as a change, so it is applied for
	 * real: window state alone leaves runtime consumers on their defaults.
	 * Ranged settings are parsed as floats: `"0.5"` read as an int is 0, out of
	 * range, and wiped on every connect. Plain values (`audio_channels`)
	 * configure pipelines rather than preferences and stay runtime-only.
	 * @param {Object<string, object>} serverSettings The payload's per-key specs.
	 * @returns {Object<string, *>} Corrections the server has to be told about.
	 */
	function sanitizeAndStoreSettings(serverSettings) {
		console.log("Sanitizing and storing settings based on server payload.");
		const changes = {};

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

	/**
	 * Turns full colour off where this engine's decoder has no 4:4:4 profile.
	 *
	 * Where the decoder has no 4:4:4 profile a full-colour stream is not a
	 * heavier picture but no picture, so the setting is dropped rather than
	 * asked for. Written to storage, which is what every payload and both
	 * dashboards read, so the toggle shows what the stream is.
	 */
	async function settleFullColorSupport() {
		if (await canDecodeFullColor()) return;
		if (!getBoolParam('video_fullcolor', false)) return;
		console.warn('[Selkies] full colour (4:4:4) is off: this browser decodes H.264 4:2:0 only.');
		setBoolParam('video_fullcolor', false);
	}

	/**
	 * Sends the persisted settings as the session's initial SETTINGS payload.
	 *
	 * Every display page sends its own: the server applies a payload to the
	 * display whose channel delivered it, so a secondary configures only its
	 * stream, its resolution still riding the resize message. Per-display keys
	 * carry a `_display2` suffix and each display reads only its own variant.
	 * Manual dimensions are exact physical pixels and go raw, as on the resize
	 * path. The DPR-derived `scaling_dpi` is seeded into this first payload
	 * unless a stored pick was collected: without it the desktop comes up at
	 * the default DPI and the dashboard's correction a second later forces a
	 * second capture restart on every HiDPI connect.
	 */
	function sendClientPersistedSettings() {
		if (isSharedMode) {
			console.log("Skipping sending client persisted settings in shared mode.");
			return;
		}
		const settingsPrefix = `${storageAppName}_`;
		const settingsToSend = {};
		const dpr = useCssScaling ? 1 : (window.devicePixelRatio || 1);

		const knownSettings = [
			'framerate', 'encoder', 'manual_resolution',
			'audio_bitrate', 'video_bitrate', 'scaling_dpi', 'enable_binary_clipboard',
			'rate_control_mode', 'video_crf', 'use_cpu', 'force_aligned_resolution',
			'video_fullcolor', 'video_streaming_mode', 'use_paint_over_quality',
			'video_paintover_crf', 'video_paintover_burst_frames'
		];
		const booleanSettingKeys = [
			'manual_resolution', 'enable_binary_clipboard', 'use_cpu',
			'video_fullcolor', 'video_streaming_mode', 'use_paint_over_quality',
			'force_aligned_resolution'
		];
		const integerSettingKeys = [
			'framerate', 'audio_bitrate', 'scaling_dpi', 'video_crf',
			'video_paintover_crf', 'video_paintover_burst_frames', 'video_bitrate'
		];

		for (const key in localStorage) {
			if (Object.hasOwnProperty.call(localStorage, key) && key.startsWith(settingsPrefix)) {
				const unprefixedKey = key.substring(settingsPrefix.length);
				let baseKey = unprefixedKey;
				if (unprefixedKey.endsWith('_display2')) {
					if (storageDisplayId !== 'display2') continue;
					baseKey = unprefixedKey.slice(0, -'_display2'.length);
				} else if (storageDisplayId === 'display2' && PER_DISPLAY_SETTINGS.includes(unprefixedKey)) {
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

		if (window.manualResolution && manualWidth != null && manualHeight != null) {
			settingsToSend['manual_resolution'] = true;
			settingsToSend['manual_width'] = alignResolution(manualWidth);
			settingsToSend['manual_height'] = alignResolution(manualHeight);
		}
		if (settingsToSend['scaling_dpi'] === undefined) {
			settingsToSend['scaling_dpi'] = scalingDPI;
		}
		if (detectedKeyboardLayout) {
			settingsToSend['keyboardLayout'] = detectedKeyboardLayout;
		}
		settingsToSend['useCssScaling'] = useCssScaling;
		// This page's remote pixels per CSS pixel, so a neighboring display can
		// scale a cross-display drag's travel over this one: the fitted video
		// box where one is measurable, else the device pixel ratio the
		// resolution request was built with.
		settingsToSend['displayScale'] = (() => {
			const v = videoElement;
			if (v && v.videoWidth > 0 && v.videoHeight > 0) {
				const r = v.getBoundingClientRect();
				const fit = Math.min(r.width / v.videoWidth, r.height / v.videoHeight);
				if (fit > 0) return 1 / fit;
			}
			return dpr;
		})();

		try {
			const settingsJson = JSON.stringify(settingsToSend);
			webrtc.sendDataChannelMessage(`SETTINGS,${settingsJson}`);
			persistentSettingsSent = true;
			console.log('Sent initial settings to server:', settingsToSend);
		} catch (e) {
			console.error('Error constructing or sending initial settings:', e);
		}
	}

	/**
	 * Sizes and centers the video element for a manual resolution; the exact
	 * size is centered too, or a larger viewport would pin the box top-left.
	 * @param {number} targetWidth Stream width in pixels.
	 * @param {number} targetHeight Stream height in pixels.
	 * @param {boolean} scaleToFit Letterbox into the container instead of showing the exact size.
	 */
	function applyManualStyle(targetWidth, targetHeight, scaleToFit) {
		if (targetWidth <=0 || targetHeight <=0) {
			console.log("Invalid target height or width")
			return;
		}

		const dpr = (window.manualResolution || useCssScaling) ? 1 : (window.devicePixelRatio || 1);
		const logicalWidth = alignResolution(targetWidth * dpr);
		const logicalHeight = alignResolution(targetHeight * dpr);
		console.log(`applyManualStyle logicalWidth: ${logicalWidth} logicalHeight: ${logicalHeight}`)
		if (videoElement.width !== logicalWidth || videoElement.height !== logicalHeight) {
			videoElement.width = logicalWidth;
			videoElement.height = logicalHeight;
			console.log(`Video Element set to: ${targetWidth}x${targetHeight}`);
		}
		const container = videoElement.parentElement;
		const containerWidth = container.clientWidth;
		const containerHeight = container.clientHeight;
		if (scaleToFit) {
			const targetAspectRatio = targetWidth / targetHeight;
			const containerAspectRatio = containerWidth / containerHeight;
			let cssWidth, cssHeight;
			if (targetAspectRatio > containerAspectRatio) {
				cssWidth = containerWidth;
				cssHeight = containerWidth / targetAspectRatio;
			} else {
				cssHeight = containerHeight;
				cssWidth = containerHeight * targetAspectRatio;
			}
			const topOffset = (containerHeight - cssHeight) / 2;
			const leftOffset = (containerWidth - cssWidth) / 2;
			videoElement.style.position = 'absolute';
			videoElement.style.width = `${cssWidth}px`;
			videoElement.style.height = `${cssHeight}px`;
			videoElement.style.top = `${topOffset}px`;
			videoElement.style.left = `${leftOffset}px`;
			videoElement.style.objectFit = 'contain';
			console.log(`Applied manual style (Scaled): CSS ${cssWidth}x${cssHeight}, Pos ${leftOffset},${topOffset}`);
		} else {
			const topOffset = (containerHeight - targetHeight) / 2;
			const leftOffset = (containerWidth - targetWidth) / 2;
			videoElement.style.position = 'absolute';
			videoElement.style.width = `${targetWidth}px`;
			videoElement.style.height = `${targetHeight}px`;
			videoElement.style.top = `${topOffset}px`;
			videoElement.style.left = `${leftOffset}px`;
			// 'fill' ignores the aspect ratio; the stream already matches the box.
			videoElement.style.objectFit = 'fill';
			console.log(`Applied manual style (Exact): CSS ${targetWidth}x${targetHeight}, Pos ${leftOffset},${topOffset}`);
		}
		updateVideoImageRendering();
	}

	/**
	 * Sizes the video element to the window: the buffer hint in physical
	 * pixels, the on-screen box in CSS pixels (styling it with physical pixels
	 * overflows the viewport by dpr squared on HiDPI displays).
	 * @param {number} targetWidth Window width in CSS pixels.
	 * @param {number} targetHeight Window height in CSS pixels.
	 */
	function resetToWindowResolution(targetWidth, targetHeight) {
		if (!videoElement) return;

		const dpr = useCssScaling ? 1 : (window.devicePixelRatio || 1);
		const logicalWidth = alignResolution(targetWidth * dpr);
		const logicalHeight = alignResolution(targetHeight * dpr);
		console.log(`resetToWinRes logicalWidth: ${logicalWidth} logicalHeight: ${logicalHeight}`)
		if (videoElement.width !== logicalWidth || videoElement.height !== logicalHeight) {
			videoElement.width = logicalWidth;
			videoElement.height = logicalHeight;
			console.log(`Video Element set to: ${logicalWidth}x${logicalHeight}`);
		}

		videoElement.style.position = 'absolute';
		videoElement.style.width = `${Math.round(targetWidth)}px`;
		videoElement.style.height = `${Math.round(targetHeight)}px`;
		videoElement.style.top = '0px';
		videoElement.style.left = '0px';
		videoElement.style.objectFit = 'fill';
		console.log(`Resized to window resolution: ${logicalWidth}x${logicalHeight} (css ${targetWidth}x${targetHeight})`);
	}

	/** The DPI slider stops, in 25% steps from 96. */
	const DPI_STOPS = [96, 120, 144, 168, 192, 216, 240, 264, 288];

	/**
	 * Derives `scaling_dpi` from the local display scaling so remote fonts
	 * match local ones: dpr 1.5 is 144, 2 is 192. Snapping to the nearest stop
	 * puts a density the stops do not name (a 3.5x phone, a 133% desktop) on
	 * the closest one and clamps at both ends.
	 * @returns {number}
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
	 * matches the display the window is on. Called from both the resize
	 * handler and the matchMedia density watcher: an OS scaling change can
	 * surface as either, and emulated density changes fire only the resize.
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
		try { webrtc.sendDataChannelMessage(`s,${derived}`); } catch (_) { /* reconnect reseeds */ }
	}

	/**
	 * Requests a stream resolution with the `r,WxH` message.
	 *
	 * A manual resolution is the exact framebuffer and is not multiplied by the
	 * device pixel ratio, or a HiDPI toggle would swing it between 1x and 2x;
	 * an auto resolution is CSS pixels times dpr unless CSS scaling is on. Both
	 * are capped at 4080 so a dpr-2 4K fullscreen never asks for a 7680-wide
	 * framebuffer.
	 * @param {number} width
	 * @param {number} height
	 */
	function sendResolutionToServer(width, height) {
		if (isSharedMode) {
			console.log("Skipping sending resolution in shared mode.");
			return;
		}
		let realWidth, realHeight, dpr;
		if (window.manualResolution) {
			dpr = 1;
			realWidth = alignResolution(width);
			realHeight = alignResolution(height);
		} else {
			dpr = useCssScaling ? 1 : (window.devicePixelRatio || 1);
			realWidth = alignResolution(width * dpr);
			realHeight = alignResolution(height * dpr);
		}
		if (realWidth > 4080) realWidth = 4080;
		if (realHeight > 4080) realHeight = 4080;
		const resString = `${realWidth}x${realHeight}`;
		lastRequestedStreamRes = [realWidth, realHeight];
		console.log(`Sending resolution to server: ${resString}, Pixel Ratio Used: ${dpr}, useCssScaling: ${useCssScaling}`);
		webrtc.sendDataChannelMessage(`r,${resString}`);
	}

	/** Follows window resizes with stream resolution requests. */
	function enableAutoResize() {
		window.addEventListener("resize", resizeStart);
	}

	/** Stops following window resizes. */
	function disableAutoResize() {
		window.removeEventListener("resize", resizeStart);
	}

	/**
	 * Manual mode detaches the auto-resize listener, but the manual style's
	 * centering offsets still depend on the container size, so they are
	 * recomputed on every window geometry change (fullscreen, resize). The
	 * listener gates itself and is registered once.
	 */
	window.addEventListener('resize', () => {
		if (window.manualResolution && !isSharedMode
			&& manualWidth > 0 && manualHeight > 0 && videoElement && videoElement.parentElement) {
			applyManualStyle(manualWidth, manualHeight, scaleLocal);
		}
	});

	/** Debounces window resizes into handleResizeUI. */
	function resizeStart() {
		maybeFollowDpr();
		rtime = new Date();
		if (rtimeout === false) {
			rtimeout = true;
			setTimeout(() => { resizeEnd() }, rdelta);
		}
	}

	/** Runs handleResizeUI once the resize has been quiet for `rdelta`. */
	function resizeEnd() {
		if (new Date() - rtime < rdelta) {
			setTimeout(() => { resizeEnd() }, rdelta);
		} else {
			rtimeout = false;
			handleResizeUI();
		}
	}

	/**
	 * Auto-mode resize: requests the window's CSS-pixel size from the server
	 * (sendResolutionToServer applies the device pixel ratio) and restyles the
	 * element onto it; shared by the debounced resize tail and the
	 * reset-to-window path.
	 *
	 * A manual preset applied while a debounce is pending is not overwritten
	 * when it fires. `enable_resize` false pins the primary's resolution
	 * server-side, so the resize the server ignores is neither requested nor
	 * restyled onto; a secondary's stays allowed. The CSS size is clamped so
	 * the physical request stays within the 4080 cap and the element box
	 * matches what the server realizes.
	 */
	function handleResizeUI() {
		if (window.manualResolution) {
			return;
		}
		if (window.enable_resize === false && storageDisplayId !== 'display2') {
			return;
		}
		windowResolution = input.getWindowResolution();
		const dpr = useCssScaling ? 1 : (window.devicePixelRatio || 1);
		if (windowResolution[0] * dpr > 4080) windowResolution[0] = Math.floor(4080 / dpr);
		if (windowResolution[1] * dpr > 4080) windowResolution[1] = Math.floor(4080 / dpr);
		sendResolutionToServer(windowResolution[0], windowResolution[1]);
		resetToWindowResolution(windowResolution[0], windowResolution[1]);
	}

	/**
	 * Re-runs the auto-resize path when the device pixel ratio changes: a
	 * window dragged to a monitor of another density, or an OS scaling change,
	 * fires no resize event, and the stream would stay at the old density
	 * until the next one. While `scaling_dpi` sits on its automatic default
	 * it is re-derived and pushed too, so the remote UI density follows the
	 * display the window is on. A matchMedia resolution query is one-shot at
	 * a given dppx, so it is re-armed after each change.
	 */
	const watchDevicePixelRatio = () => {
		let mql = null;
		const onDprChange = () => {
			maybeFollowDpr();
			if (!window.manualResolution && !isSharedMode) { resizeStart(); }
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

	/**
	 * Restores the last session's resolution and desktop DPI on connect.
	 *
	 * The DPI goes through the server's idempotent `set_dpi` path, the same one
	 * the initial SETTINGS seed takes, so whichever lands first wins. Persisted
	 * trackpad mode re-asserts cursor compositing (touch has no hover cursor).
	 * A manual-mode secondary display reports its size on connect because a
	 * secondary lays out from what it reports; a pinned primary
	 * (`enable_resize` false) is styled to the window but keeps the server's
	 * resolution.
	 */
	function loadLastSessionSettings() {
		if (isSharedMode) {
			console.log("Skipping loading last session settings in shared mode.");
			return;
		}
		if (webrtc) { try { webrtc.sendDataChannelMessage(`s,${scalingDPI}`); } catch (_) {} }
		if (trackpadMode && webrtc) {
			try { webrtc.sendDataChannelMessage('SET_NATIVE_CURSOR_RENDERING,1'); } catch (_) {}
		}
		if (window.manualResolution && manualWidth && manualHeight) {
			console.log(`Applying manual resolution: ${manualWidth}x${manualHeight}`);
			applyManualStyle(manualWidth, manualHeight, scaleLocal);
			if (window.location.hash.startsWith('#display2')) {
				sendResolutionToServer(manualWidth, manualHeight);
			}
		} else {
			console.log("Applying window resolution");
			const currentWindowRes = input.getWindowResolution();
			resetToWindowResolution(...currentWindowRes);
			if (window.enable_resize !== false || storageDisplayId === 'display2') {
				sendResolutionToServer(currentWindowRes[0], currentWindowRes[1]);
			}
			enableAutoResize();
		}
	}

	/** Posts `sidebarButtonStatusUpdate` with the state of every pipeline toggle. */
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
	 * Starts the webcam uplink: the camera track rides the sendonly video
	 * transceiver the server reserved in the bundled SDP (the mirror of the
	 * microphone), so the browser's own encoder produces the H.264 or VP8 the
	 * server's virtual camera decodes, with RTP congestion control and no
	 * data-channel framing.
	 */
	async function startWebcamCapture() {
		if (isSharedMode || !webrtc || isWebcamActive || webcamBusy) {
			return;
		}
		webcamBusy = true;
		try {
			const ok = await webrtc.setWebcam(true, preferredWebcamDeviceId);
			if (ok) {
				const track = webrtc.webcamTrack;
				if (track) {
					track.addEventListener('ended', () => {
						if (isWebcamActive) stopWebcamCapture();
					});
				}
				isWebcamActive = true;
			} else {
				isWebcamActive = false;
			}
		} catch (error) {
			console.error('Webcam capture error:', error);
			isWebcamActive = false;
		} finally {
			webcamBusy = false;
		}
		postSidebarButtonUpdate();
	}

	/** Stops the webcam uplink and reports the toggle state. */
	function stopWebcamCapture() {
		if (webrtc) {
			webrtc.setWebcam(false).catch(() => {});
		}
		if (isWebcamActive) {
			isWebcamActive = false;
			postSidebarButtonUpdate();
		}
	}

	/**
	 * Applies the gamepad toggle to the manager; a shared page always polls.
	 * @returns {boolean} Whether polling is on.
	 */
	function toggleGamepadConnection() {
		if (input && input.gamepadManager) {
			if (isSharedMode) {
				input.gamepadManager.enable();
				console.log("Shared mode: Gamepad control message received, ensuring its GamepadManager remains active for polling.");
				return true;
			} else {
				if (isGamepadEnabled) {
					input.gamepadManager.enable();
					console.log("Primary mode: Gamepad toggle ON. Enabling GamepadManager polling.");
					return true;
				} else {
					input.gamepadManager.disable();
					console.log("Primary mode: Gamepad toggle OFF. Disabling GamepadManager polling.");
				}
			}
		} else {
			console.warn("Client: input.gamepadManager not found in 'gamepadControl' message handler");
		}
		return false;
	}

	/**
	 * Handles a same-origin dashboard window message; the module docblock
	 * lists the types. A shared page ignores the resolution, command and
	 * clipboard cases: a viewer never drives resolution policy, never reaches
	 * the server's command execution path and never writes its clipboard.
	 * @param {MessageEvent} event
	 */
	function handleMessage(event) {
		if (event.origin !== window.location.origin) {
			console.warn("Received message from unexpected origin");
			return;
		}
		let message = event.data;
		switch(message.type) {
			case "setScaleLocally":
				if (isSharedMode) { break; }
				if (typeof message.value === 'boolean') {
					scaleLocal = message.value;
					setBoolParam("scaleLocallyManual", scaleLocal);
					console.log(`Set scaleLocallyManual to ${scaleLocal} and persisted.`);
					if (window.manualResolution && manualWidth && manualHeight) {
						applyManualStyle(manualWidth, manualHeight, scaleLocal);
					}
				} else {
					console.warn("Invalid value received for setScaleLocally:", message.value);
				}
				break;
			case "resetResolutionToWindow":
				if (isSharedMode) { break; }
				console.log("Resetting to window size");
				// The flag drops first so the re-send takes the auto path (window size times dpr).
				window.manualResolution = false;
				manualHeight = manualWidth = 0;
				setIntParam('manual_width', null);
				setIntParam('manual_height', null);
				setBoolParam('manual_resolution', false);
				enableAutoResize();
				handleResizeUI();
				break;
			case "setManualResolution":
				if (isSharedMode) { break; }
				const width = parseInt(message.width, 10);
				const height = parseInt(message.height, 10);
				if (isNaN(width) || width <= 0 || isNaN(height) || height <= 0) {
					console.error('Received invalid width/height for setManualResolution:', message);
					break;
				}
				console.log(`Setting manual resolution: ${width}x${height}`);
				// The flag rises before the send: a preset is exact framebuffer pixels, which the
				// auto path would multiply by dpr.
				window.manualResolution = true;
				manualWidth = width;
				manualHeight = height;
				setIntParam('manual_width', manualWidth);
				setIntParam('manual_height', manualHeight);
				setBoolParam('manual_resolution', true);
				disableAutoResize();
				sendResolutionToServer(manualWidth, manualHeight);
				applyManualStyle(manualWidth, manualHeight, scaleLocal);
				break;
			case "setUseCssScaling":
				if (isSharedMode) { break; }
				if (typeof message.value === 'boolean') {
					const changed = useCssScaling !== message.value;
					useCssScaling = message.value;
					// persist === false is a server-authored value: applied, but the user's own key
					// is left alone.
					if (message.persist !== false) {
						setBoolParam('useCssScaling', useCssScaling);
					}
					console.log(`Set useCssScaling to ${useCssScaling}${message.persist === false ? '.' : ' and persisted.'}`);
					if (input && typeof input.updateCssScaling === 'function') {
						input.updateCssScaling(useCssScaling);
					}
					if (changed) {
						updateVideoImageRendering();
						if (window.manualResolution && manualWidth != null && manualHeight != null) {
							sendResolutionToServer(manualWidth, manualHeight);
							applyManualStyle(manualWidth, manualHeight, scaleLocal);
						} else if (!isSharedMode && input &&
							(window.enable_resize !== false || storageDisplayId === 'display2')) {
							// A pinned primary keeps its resolution; secondaries stay allowed.
							const currentWindowRes = input.getWindowResolution();
							const autoWidth = alignResolution(currentWindowRes[0]);
							const autoHeight = alignResolution(currentWindowRes[1]);
							sendResolutionToServer(autoWidth, autoHeight);
							resetToWindowResolution(autoWidth, autoHeight);
						}
					}
				} else {
					console.warn("Invalid value received for setUseCssScaling:", message.value);
				}
				break;
			case "settings":
				console.log("Received settings msg from dashboard:", message.settings);
				handleSettingsMessage(message.settings);
				break;
			case "command":
				if (isSharedMode) { break; }
				if (!serverCommandEnabled) {
					console.log("Command sending suppressed: server has command_enabled=false; not sending 'cmd,'.");
					break;
				}
				// A real value only, never the string "null" or "undefined".
				if (message.value !== null && message.value !== undefined) {
					const commandString = message.value;
					console.log(`Received 'command' message with value: "${commandString}"`);
					webrtc.sendDataChannelMessage(`cmd,${commandString}`);
				} else {
					console.warn(`Received invalid command from dashboard: ${message.value}`)
				}
				break;
			case 'pipelineControl':
				if (message.pipeline === 'microphone' && isSharedMode) {
					console.log("Shared mode: Microphone control blocked.");
					break;
				}
				if (message.pipeline === 'microphone' && webrtc && typeof webrtc.setMicrophone === 'function') {
					const micOn = !!message.enabled;
					webrtc.setMicrophone(micOn, preferredInputDeviceId).then(() => {
						isMicrophoneActive = micOn;
						postSidebarButtonUpdate();
					}).catch((e) => {
						console.error('Microphone toggle failed:', e);
						isMicrophoneActive = false;
						postSidebarButtonUpdate();
					});
				} else if (message.pipeline === 'video' && isSharedMode) {
					console.log("Shared mode: Video pipelineControl blocked.");
					break;
				} else if (message.pipeline === 'video' && webrtc) {
					const videoOn = !!message.enabled;
					try {
						webrtc.sendDataChannelMessage(videoOn ? 'START_VIDEO' : 'STOP_VIDEO');
						isVideoPipelineActive = videoOn;
						window.postMessage({ type: 'pipelineStatusUpdate', video: videoOn }, window.location.origin);
						postSidebarButtonUpdate();
					} catch (e) {
						console.error('Video toggle failed:', e);
					}
				} else if (message.pipeline === 'audio' && videoElement) {
					// Audio stays negotiated; the toggle only mutes the element carrying it.
					const audioOn = !!message.enabled;
					videoElement.muted = !audioOn;
					isAudioPipelineActive = audioOn;
					window.postMessage({ type: 'pipelineStatusUpdate', audio: audioOn }, window.location.origin);
					postSidebarButtonUpdate();
				} else if (message.pipeline === 'webcam') {
					if (isSharedMode) {
						console.log("Shared mode: Webcam control blocked.");
						break;
					}
					if (!!message.enabled) {
						startWebcamCapture();
					} else {
						stopWebcamCapture();
					}
				}
				break;
			case 'gamepadControl':
				console.log(`Received gamepad control message: enabled=${message.enabled}`);
				const newGamepadState = message.enabled;
				if (isGamepadEnabled !== newGamepadState) {
					isGamepadEnabled = newGamepadState;
					setBoolParam('isGamepadEnabled', isGamepadEnabled);
					postSidebarButtonUpdate();
					toggleGamepadConnection()
				}
				break;
			case 'clipboardUpdateFromUI':
				console.log('Received clipboardUpdateFromUI message.');
				if (isSharedMode) {
					console.log("Shared mode: Clipboard write to server blocked.");
					break;
				}
				localClipboardSender.sendExplicit(message.text);
				break;
			case 'clipboardImageUpdate': {
				// Every skip surfaces a notification: a dead click reads as a bug.
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
				localClipboardSender.sendExplicit(
					message.imageBlob, message.imageBlob.type || 'image/png', notifyClipboardImageSkip
				).catch((e) => {
					console.warn('Failed to send uploaded clipboard image:', e);
					notifyClipboardImageSkip('send failed: ' + e.message, 'clipboardSkipSendFailed');
				});
				break;
			}
			case 'audioDeviceSelected':
				if (message.context === 'output' && message.deviceId) {
					preferredOutputDeviceId = message.deviceId;
					applyOutputDevice();
				} else if (message.context === 'input' && message.deviceId) {
					preferredInputDeviceId = message.deviceId;
					if (isMicrophoneActive && webrtc && typeof webrtc.setMicrophone === 'function') {
						webrtc.setMicrophone(false).then(() =>
							webrtc.setMicrophone(true, preferredInputDeviceId)
						).catch((e) => {
							console.error('Microphone device switch failed:', e);
							isMicrophoneActive = false;
							postSidebarButtonUpdate();
						});
					}
				}
				break;
			case 'requestFullscreen':
			case 'requestGamingMode': {
				// Plain fullscreen leaves the pointer and the keyboard to the browser; gaming mode
				// holds both.
				const gaming = message.type === 'requestGamingMode';
				gamingModeActive = gaming;
				if (input && gaming && typeof input.enterGamingMode === 'function') {
					input.enterGamingMode();
				} else if (input) {
					input.enterFullscreen();
				} else if (document.fullscreenElement === null) {
					document.documentElement.requestFullscreen().catch(() => {});
				}
				break;
			}
			case 'setSynth':
				if (input && typeof input.setSynth === 'function') {
					input.setSynth(message.value);
				}
				break;
			case 'showVirtualKeyboard': {
				// Focusing the off-screen assist input opens the mobile soft keyboard; the next
				// touch of the stream blurs it.
				if (isSharedMode) { break; }
				const kbdAssistInput = document.getElementById('keyboard-input-assist');
				const mainInteractionOverlay = document.getElementById('overlayInput');
				if (kbdAssistInput) {
					kbdAssistInput.value = '';
					kbdAssistInput.focus();
					if (mainInteractionOverlay) {
						mainInteractionOverlay.addEventListener('touchstart', () => {
							if (document.activeElement === kbdAssistInput) { kbdAssistInput.blur(); }
						}, { once: true, passive: true });
					}
				}
				break;
			}
			case 'setAntiAliasing':
				if (typeof message.value === 'boolean') {
					antiAliasingEnabled = message.value;
					setBoolParam('antiAliasingEnabled', antiAliasingEnabled);
					updateVideoImageRendering();
				} else {
					console.warn("Invalid value received for setAntiAliasing:", message.value);
				}
				break;
			case 'setUseBrowserCursors':
				if (typeof message.value === 'boolean') {
					useBrowserCursors = message.value;
					setBoolParam('use_browser_cursors', message.value);
					applyEffectiveCursorSetting();
				} else {
					console.warn("Invalid value received for setUseBrowserCursors:", message.value);
				}
				break;
			case 'touchinput:trackpad':
				if (input && typeof input.setTrackpadMode === 'function') {
					trackpadMode = true;
					setBoolParam('trackpadMode', true);
					input.setTrackpadMode(true);
					// Touch has no hover cursor: the pointer is composited into the video.
					if (webrtc) {
						try { webrtc.sendDataChannelMessage('SET_NATIVE_CURSOR_RENDERING,1'); } catch (_) {}
					}
				}
				break;
			case 'touchinput:touch':
				if (input && typeof input.setTrackpadMode === 'function') {
					trackpadMode = false;
					setBoolParam('trackpadMode', false);
					input.setTrackpadMode(false);
					if (webrtc) {
						try { webrtc.sendDataChannelMessage('SET_NATIVE_CURSOR_RENDERING,0'); } catch (_) {}
					}
				}
				break;
			default:
				break;
		}
	}

	/**
	 * Applies a settings payload from the dashboard or the server.
	 *
	 * A server-authored payload (the locked and overridden values replayed on
	 * every connect) is applied to the runtime but never written to the user's
	 * own keys, where it would outlive the lock and masquerade as their pick;
	 * only a dashboard-authored payload persists. Settings with no dedicated
	 * data-channel opcode ride a SETTINGS passthrough the server applies through
	 * `handle_update_settings`; `displayPosition` among them moves a secondary
	 * page to another side of the primary, and the primary ignores it.
	 * @param {Object<string, *>} settings Keys named as the server names them.
	 * @param {boolean} [fromServer] Whether the server authored the payload.
	 */
	function handleSettingsMessage(settings, fromServer) {
		const storeInt = fromServer ? () => {} : setIntParam;
		const storeBool = fromServer ? () => {} : setBoolParam;
		const storeString = fromServer ? () => {} : setStringParam;
		if (settings.debug !== undefined) {
			debug = settings.debug;
			// Persisted even from the server: the reload only settles once the flag is in storage.
			setBoolParam('debug', debug);
			console.log(`Applied debug setting: ${debug}. Reloading...`);
			setTimeout(() => { window.location.reload(); }, 700);
			return;
		}
		const passthrough = {};
		if (settings.video_fullcolor !== undefined) passthrough.video_fullcolor = !!settings.video_fullcolor;
		if (settings.video_streaming_mode !== undefined) passthrough.video_streaming_mode = !!settings.video_streaming_mode;
		if (settings.use_paint_over_quality !== undefined) passthrough.use_paint_over_quality = !!settings.use_paint_over_quality;
		if (settings.video_paintover_crf !== undefined) passthrough.video_paintover_crf = parseInt(settings.video_paintover_crf, 10);
		if (settings.video_paintover_burst_frames !== undefined) passthrough.video_paintover_burst_frames = parseInt(settings.video_paintover_burst_frames, 10);
		if (settings.force_aligned_resolution !== undefined) passthrough.force_aligned_resolution = !!settings.force_aligned_resolution;
		if (settings.use_cpu !== undefined) passthrough.use_cpu = !!settings.use_cpu;
		if (settings.encoder !== undefined) passthrough.encoder = settings.encoder;
	if (settings.displayPosition !== undefined) passthrough.displayPosition = settings.displayPosition;
		if (Object.keys(passthrough).length > 0) {
			webrtc.sendDataChannelMessage(`SETTINGS,${JSON.stringify(passthrough)}`);
		}
		if (settings.video_bitrate !== undefined) {
			videoBitRate = parseInt(settings.video_bitrate, 10);
			webrtc.sendDataChannelMessage(`vb,${videoBitRate}`);
			storeInt('video_bitrate', videoBitRate);
		}
		if (settings.framerate !== undefined) {
			videoFramerate = parseInt(settings.framerate);
			webrtc.sendDataChannelMessage(`_arg_fps,${videoFramerate}`);
			storeInt('framerate', videoFramerate);
		}
		if (settings.audio_bitrate !== undefined) {
			audioBitRate = parseInt(settings.audio_bitrate);
			webrtc.sendDataChannelMessage(`ab,${audioBitRate}`);
			storeInt('audio_bitrate', audioBitRate);
		}
		if (settings.encoder !== undefined) {
			// The pipeline restart rides the passthrough; tracked here for the decode path.
			encoder = settings.encoder;
			storeString('encoder', encoder);
			console.log("Encoder switched to:", encoder);
		}
		if (settings.scaling_dpi !== undefined) {
			const dpi = parseInt(settings.scaling_dpi, 10);
			if (!isNaN(dpi) && dpi > 0) {
				// Not persisted: the pin belongs to the dashboard's explicit slider pick; pinning
				// every post would freeze the DPI across displays of different devicePixelRatio.
				scalingDPI = dpi;
				webrtc.sendDataChannelMessage(`s,${dpi}`);
			}
		}
		if (settings.enable_binary_clipboard !== undefined) {
			enable_binary_clipboard = !!settings.enable_binary_clipboard;
			webrtc.sendDataChannelMessage(`_ebc,${enable_binary_clipboard}`);
			storeBool('enable_binary_clipboard', enable_binary_clipboard);
			console.log(`Binary clipboard support ${enable_binary_clipboard ? 'enabled' : 'disabled'}`);
		}
		if (settings.clipboard_in_enabled !== undefined) {
			clipboard_in_enabled = !!settings.clipboard_in_enabled;
			storeBool('clipboard_in_enabled', clipboard_in_enabled);
		}
		if (settings.clipboard_out_enabled !== undefined) {
			clipboard_out_enabled = !!settings.clipboard_out_enabled;
			storeBool('clipboard_out_enabled', clipboard_out_enabled);
		}
		if (settings.use_css_scaling !== undefined) {
			// Routed through the dashboard toggle's flow so the value reaches every layer.
			handleMessage({
				origin: window.location.origin,
				data: { type: 'setUseCssScaling', value: !!settings.use_css_scaling, persist: !fromServer },
			});
		}
		if (settings.use_browser_cursors !== undefined) {
			// Never persisted: only the setUseBrowserCursors message persists.
			useBrowserCursors = !!settings.use_browser_cursors;
			applyEffectiveCursorSetting();
		}
		if (settings.rate_control_mode !== undefined) {
			rateControlMode = settings.rate_control_mode;
			webrtc.sendDataChannelMessage(`_rc,${rateControlMode}`);
			sendRespectiveRCvalue(rateControlMode);
			storeString('rate_control_mode', rateControlMode);
			console.log(`Rate control mode set to ${rateControlMode}`);
		}
		if (settings.video_crf !== undefined) {
			crf = parseInt(settings.video_crf, 10);
			webrtc.sendDataChannelMessage(`_crf,${crf}`);
			storeInt('video_crf', crf);
			console.log(`H264 CRF set to ${crf}`);
		}
		if (settings.force_aligned_resolution !== undefined) {
			force_aligned_resolution = !!settings.force_aligned_resolution;
			storeBool('force_aligned_resolution', force_aligned_resolution);
			// Re-sent so the stream snaps to the new alignment; a pinned primary is left alone.
			if (window.manualResolution && manualWidth != null && manualHeight != null) {
				sendResolutionToServer(manualWidth, manualHeight);
			} else if (!isSharedMode && input &&
				(window.enable_resize !== false || storageDisplayId === 'display2')) {
				const currentWindowRes = input.getWindowResolution();
				sendResolutionToServer(currentWindowRes[0], currentWindowRes[1]);
			}
		}
	}

	/** Re-sends the parameter the new rate-control mode reads: the bitrate for CBR, the CRF for CRF. */
	function sendRespectiveRCvalue(newMode) {
		if (newMode === "cbr") {
			webrtc.sendDataChannelMessage(`vb,${videoBitRate}`);
		} else if (newMode === "crf") {
			webrtc.sendDataChannelMessage(`_crf,${crf}`);
		}
	};

	/** HTTP uploads and the drag-drop and file-picker plumbing (`lib/file-upload.js`); shared sessions never upload. */
	const fileUploader = createFileUploader({ canUpload: () => !isSharedMode });
	const handleRequestFileUpload = fileUploader.handleRequestFileUpload;
	const handleFileInputChange = fileUploader.handleFileInputChange;
	const handleDragOver = fileUploader.handleDragOver;
	const handleDrop = fileUploader.handleDrop;

	/**
	 * Starts the once-a-second stats loop: the essentials are published on
	 * `window` (`fps`, `network_stats`, `currentAudioBufferSize`) for the
	 * dashboards, the full `connectionStat` stays readable here, and
	 * `enableWebrtcStatics` streams the raw reports to the server as
	 * `_stats_video`.
	 *
	 * A tick whose predecessor still awaits `getStats()` is skipped, since
	 * overlapping ticks would double-update the byte baselines, and the time
	 * window is re-anchored only on success, alongside those baselines, so
	 * both cover the same interval. The bandwidth reported is the received
	 * throughput (video plus audio), matching the WebSocket server's stat:
	 * `availableReceiveBandwidth` is only the congestion-control estimate and
	 * reads far below the real rate on a relay. The audio-buffer gauge is a
	 * proxy: the de-jitter depth over the 20 ms Opus frame approximates the
	 * frames buffered ahead of playout, since browser-managed audio exposes no
	 * frame count. The audio concealment counters (NetEQ) are the RED
	 * acceptance metric.
	 */
	function enableStatWatch() {
		if (isSharedMode) {
			console.log("Shared mode detected, skipping stats watch setup.");
			return;
		}
		var videoBytesReceivedStart = 0;
		var audioBytesReceivedStart = 0;
		var previousVideoJitterBufferDelay = 0.0;
		var previousVideoJitterBufferEmittedCount = 0;
		var previousAudioJitterBufferDelay = 0.0;
		var previousAudioJitterBufferEmittedCount = 0;
		var statsStart = new Date().getTime() / 1000;
		if (statsLoopId !== null) return;
		statWatchEnabled = true;
		let statsTickBusy = false;
		statsLoopId = setInterval(async () => {
			if (statsTickBusy) return;
			statsTickBusy = true;
			var now = new Date().getTime() / 1000;
			try {
				const stats = await webrtc.getConnectionStats();
				connectionStat = {};

				const rtt = (stats.general.currentRoundTripTime !== null) ? (stats.general.currentRoundTripTime * 1000.0) : (serverLatency)

				connectionStat.connectionPacketsReceived = stats.general.packetsReceived;
				connectionStat.connectionPacketsLost = stats.general.packetsLost;
				connectionStat.connectionStatType = stats.general.connectionType
				connectionStat.connectionBytesReceived = (stats.general.bytesReceived * 1e-6).toFixed(2) + " MBytes";
				connectionStat.connectionBytesSent = (stats.general.bytesSent * 1e-6).toFixed(2) + " MBytes";
				connectionStat.connectionAvailableBandwidth = (parseInt(stats.general.availableReceiveBandwidth) / 1e+6).toFixed(2) + " mbps";

				connectionStat.connectionCodec = stats.video.codecName;
				connectionStat.connectionVideoDecoder = stats.video.decoder;
				connectionStat.connectionResolution = stats.video.frameWidth + "x" + stats.video.frameHeight;
				connectionStat.connectionFrameRate = stats.video.framesPerSecond;
				connectionStat.connectionVideoBitrate = (((stats.video.bytesReceived - videoBytesReceivedStart) / (now - statsStart)) * 8 / 1e+6).toFixed(2);
				videoBytesReceivedStart = stats.video.bytesReceived;

				connectionStat.connectionAudioCodecName = stats.audio.codecName;
				connectionStat.connectionAudioBitrate = (((stats.audio.bytesReceived - audioBytesReceivedStart) / (now - statsStart)) * 8 / 1e+3).toFixed(2);
				audioBytesReceivedStart = stats.audio.bytesReceived;
				connectionStat.connectionAudioConcealedSamples = stats.audio.concealedSamples;
				connectionStat.connectionAudioConcealmentEvents = stats.audio.concealmentEvents;
				connectionStat.connectionAudioTotalSamplesReceived = stats.audio.totalSamplesReceived;
				connectionStat.connectionAudioPacketsDiscarded = stats.audio.packetsDiscarded;
				statsStart = now;

				connectionStat.connectionVideoLatency = parseInt(Math.round(rtt + (1000.0 * (stats.video.jitterBufferDelay - previousVideoJitterBufferDelay) / (stats.video.jitterBufferEmittedCount - previousVideoJitterBufferEmittedCount) || 0)));
				previousVideoJitterBufferDelay = stats.video.jitterBufferDelay;
				previousVideoJitterBufferEmittedCount = stats.video.jitterBufferEmittedCount;
				connectionStat.connectionAudioLatency = parseInt(Math.round(rtt + (1000.0 * (stats.audio.jitterBufferDelay - previousAudioJitterBufferDelay) / (stats.audio.jitterBufferEmittedCount - previousAudioJitterBufferEmittedCount) || 0)));
				const _audioJitterMs = 1000.0 * (stats.audio.jitterBufferDelay - previousAudioJitterBufferDelay) / (stats.audio.jitterBufferEmittedCount - previousAudioJitterBufferEmittedCount) || 0;
				window.currentAudioBufferSize = Math.max(0, Math.round(_audioJitterMs / 20));
				previousAudioJitterBufferDelay = stats.audio.jitterBufferDelay;
				previousAudioJitterBufferEmittedCount = stats.audio.jitterBufferEmittedCount;

				connectionStat.connectionLatency =  Math.max(connectionStat.connectionVideoLatency, connectionStat.connectionAudioLatency);

				window.fps = connectionStat.connectionFrameRate;
				window.network_stats = {
					"bandwidth_mbps": (parseFloat(connectionStat.connectionVideoBitrate) || 0) + (parseFloat(connectionStat.connectionAudioBitrate) || 0) / 1000,
					"latency_ms": connectionStat.connectionLatency,
				};
				if (enableWebrtcStatics) webrtc.sendDataChannelMessage(`_stats_video,${JSON.stringify(stats.allReports)}`);
			} catch (e) {
				if (webrtc !== null) console.warn("Error collecting connection stats:", e);
			} finally {
				statsTickBusy = false;
			}
		}, 1000);
	}

	/**
	 * Focus and gesture local-to-server clipboard sync (`lib/clipboard-sync.js`),
	 * with text re-sends deduped. Every read is gated on the server's clipboard
	 * policy as well as browser capability, so a clipboard-disabled server never
	 * arms the focus read or its permission prompt.
	 */
	const localClipboardSender = createLocalClipboardSender({
		isChromium,
		getDeferredWriteInFlight: () => deferredClipboardWriter.getInFlight(),
		isSharedMode: () => isSharedMode,
		canSync: () => clipboardStatus === "enabled" && !!window.clipboard_enabled,
		canRead: () => !!clipboard_in_enabled,
		binaryEnabled: () => !!enable_binary_clipboard,
		sendClipboardData: (data, mime, onSkip) => sendClipboardData(data, mime, onSkip),
		dedupeText: true,
	});
	const readLocalClipboardAndSend = () => localClipboardSender.readAndSend();
	const maybeSendInitialClipboard = () => localClipboardSender.maybeInitial();

	/** Paste-ordering hold and non-Chromium copy/paste gestures (`lib/clipboard-sync.js`), wired with the session. */
	const clipboardGestures = createClipboardGestures({
		isChromium,
		clipboardSync,
		sendClipboardData: (data, mime) => sendClipboardData(data, mime),
		canSync: () => !isSharedMode && clipboardStatus === "enabled" && !!window.clipboard_enabled,
		canRead: () => !!clipboard_in_enabled,
		canWrite: () => !!clipboard_out_enabled,
		binaryEnabled: () => !!enable_binary_clipboard,
		getSendInFlight: () => localClipboardSender.getSendInFlight(),
		getDeferredWriteInFlight: () => deferredClipboardWriter.getInFlight(),
	});

	/**
	 * Releases every key server-side and, on Chromium, reads the local
	 * clipboard: Firefox and WebKit raise a paste prompt on every focus read,
	 * so there the read is driven only by the paste gesture handlers.
	 */
	async function handleWindowFocus() {
		webrtc.sendDataChannelMessage("kr");
		if (isChromium) {
			readLocalClipboardAndSend();
		}
	}


	/** Releases every key server-side so none sticks across the blur. */
	function handleWindowBlur() {
		webrtc.sendDataChannelMessage("kr");
	}

	/**
	 * Forwards the control keys mobile keyboards emit as keydown on the
	 * off-screen assist input (Enter, Backspace); typed characters are handled
	 * by the Input class's own listener on the element.
	 */
	function setupKeyBoardAssisstant() {
		if (isSharedMode) {
			console.log("Shared mode detected, skipping keyboard assistant setup.");
			return;
		}
		const keyboardInputAssist = document.getElementById('keyboard-input-assist');
		if (keyboardInputAssist && input) {
		keyboardInputAssist.addEventListener('keydown', (event) => {
			if (event.key === 'Enter' || event.keyCode === 13) {
			input._sendMomentaryKey(0xFF0D);
			event.preventDefault();
			keyboardInputAssist.value = '';
			} else if (event.key === 'Backspace' || event.keyCode === 8) {
			input._sendMomentaryKey(0xFF08);
			event.preventDefault();
			}
		});
		console.log("Added 'input' and 'keydown' listeners to #keyboard-input-assist.");
		} else {
			console.error(" Could not add listeners to keyboard assist: Element or Input handler instance not found.");
		}
	}

	/**
	 * Tells the dashboard why a clipboard-image upload was skipped, in the
	 * `fileUpload` warning channel transfer warnings use.
	 * @param {string} reason Human-readable reason.
	 * @param {string} code Machine-readable code the dashboard translates.
	 */
	function notifyClipboardImageSkip(reason, code) {
		console.warn('Clipboard image upload skipped: ' + reason);
		window.postMessage({
			type: 'fileUpload',
			payload: { status: 'warning', fileName: 'clipboard-image', message: reason, code },
		}, window.location.origin);
	}

	/**
	 * Tells the dashboard that a server image never reached the local
	 * clipboard. The panel shows nothing of an inbound image but this notice,
	 * so a write the browser refuses would otherwise read as the feature not
	 * working at all.
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
	 * Sends clipboard content to the server in chunks.
	 *
	 * Uses the chunked transfer of `lib/clipboard-worker-bridge.js`, the same
	 * wire protocol as the WebSocket core, over the data channel with a drain
	 * gate: a multi-MB burst overflows the SCTP send buffer and Chromium closes
	 * the channel, taking the session with it. Raw chunks are sized so their
	 * base64 fits the data-channel message budget. Only a completed transfer
	 * marks the content synced, so a failure leaves it re-sendable.
	 * @param {string|ArrayBuffer|Uint8Array} data Text or binary content.
	 * @param {string} [mimeType] Content type; text is always `text/plain`.
	 * @param {?function(string, string): void} [onSkip] Called with reason and
	 *     code when nothing is sent.
	 */
	async function sendClipboardData(data, mimeType = 'text/plain', onSkip = null) {
		const skip = (reason, code) => { if (onSkip) onSkip(reason, code); };
		if (data == null) {
			skip('nothing to send', 'clipboardSkipNoImage');
			return;
		}
		if (clipboardStatus !== "enabled" || window.clipboard_enabled === undefined) {
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
		if (!webrtc || !webrtc.dataChannelOpen()) {
			skip('not connected', 'clipboardSkipNotConnected');
			return;
		}
		const isBinary = data instanceof ArrayBuffer || data instanceof Uint8Array;
		let dataBytes;
		if (isBinary) {
			dataBytes = data instanceof Uint8Array ? data : new Uint8Array(data);
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
		try {
			await sendClipboardChunked(dataBytes, mimeType, {
				worker: clipboardWorker,
				send: (m) => webrtc.sendDataChannelMessage(m),
				waitDrain: async () => {
					if (webrtc.waitForDataChannelDrain) {
						await webrtc.waitForDataChannelDrain(CLIPBOARD_BACKLOG_BYTES);
					}
					return true;
				},
				chunkRawBytes: Math.min(CLIPBOARD_CHUNK_SIZE,
					Math.max(1, Math.floor(dcMessageBudget() * 3 / 4))),
				nextTid: () => ++__clipboardTransferCounter,
			});
			// A closed channel drops sends quietly, so a mid-transfer death throws nothing.
			if (!webrtc.dataChannelOpen()) {
				skip('connection lost during send', 'clipboardSkipSendFailed');
				return;
			}
			clipboardSync.markSynced(subject, mimeType);
		} catch (err) {
			console.error("Error sending clipboard data:", err);
			skip('send failed: ' + (err && err.message ? err.message : err),
				'clipboardSkipSendFailed');
		}
	}

	/**
	 * Decodes a server clipboard message, assembling multipart transfers.
	 * @param {{type: string, data: object}} msg The `clipboard-msg*` message.
	 * @returns {Promise<{isMultipart: boolean, mimeType: ?string, content: ?(string|ClipboardItem)}>}
	 *     `content` is null while a multipart transfer is in progress, on
	 *     failure, and for images on insecure origins, which have no
	 *     ClipboardItem.
	 */
	async function handleClipboardData(msg) {
		if (!msg.data) {
			console.warn("Received clipboard message with null data");
			return { isMultipart: false, mimeType: null, content: null };
		}
	
		let mimeType = msg.data.mime_type || multipartClipboard.mimeType;
		let is_text =  mimeType === 'text/plain' ? true : false;
		let content = null;
		let isMultipart = false;
		switch (msg.type) {
			case "clipboard-msg":
				let blob;
				try {
					const { result } = await clipboardWorker.decode(msg.data.content, mimeType);
					if (is_text) {
						return { isMultipart, mimeType, content: result };
					}
					blob = new Blob([result], { type: mimeType });
					if (mimeType.startsWith('image/') && mimeType !== 'image/png') {
						// ClipboardItem accepts only image/png on write.
						blob = await reencodePngOffThread(blob);
						mimeType = 'image/png';
					}
				} catch (err) {
					console.error("Image conversion failed for clipboard message:", err);
					return { isMultipart, mimeType, content: null };
				}
				if (typeof ClipboardItem === 'undefined') return { isMultipart, mimeType, content: null };
				return { isMultipart, mimeType, content: new ClipboardItem({ [mimeType]: blob }) };
			case "clipboard-msg-start":
				multipartClipboard.begin(mimeType, msg.data.total_size);
				console.log(`Starting multi-part download: ${mimeType}, expected raw size: ${msg.data.total_size}`);
				return { isMultipart: true, mimeType, content: null };
			case "clipboard-msg-data":
				multipartClipboard.push(msg.data.content);
				return { isMultipart: true, mimeType, content: null };
			case "clipboard-msg-end":
				if (!multipartClipboard.inProgress) {
					return { isMultipart: false, mimeType, content: null };
				}
				mimeType = multipartClipboard.mimeType;
				const declared = multipartClipboard.totalSize;
				try {
					const { result, byteLength } = await multipartClipboard.finish();
					if (byteLength !== declared) {
						console.warn(`Size mismatch! Expected ${declared}, got ${byteLength}`);
						return { isMultipart: false, mimeType, content: null };
					}
					if (mimeType === 'text/plain') {
						content = result;
					} else if (typeof ClipboardItem === 'undefined') {
						content = null;
					} else {
						let blob = new Blob([result], { type: mimeType });
						if (mimeType.startsWith('image/') && mimeType !== 'image/png') {
							blob = await reencodePngOffThread(blob);
							mimeType = 'image/png';
						}
						content = new ClipboardItem({ [mimeType]: blob });
					}
				} catch (err) {
					console.error("Worker decoding failed:", err);
				}
				return { isMultipart: false, mimeType, content };
			default:
				console.warn("Unknown clipboard cmd received");
		}
	}


	return {
		/**
		 * Builds the DOM, reads the persisted settings, connects signaling and
		 * opens the peer connection. Settings are read with fallbacks and never
		 * written back, so a fresh profile keeps every key unset and
		 * server-pushed defaults stay re-pushable.
		 */
		initialize() {
			InitUI();
			const appDiv = document.getElementById('app');
			let videoContainer = document.createElement("div");
			videoContainer.className = "video-container";

			playButtonElement = document.createElement('button');
			playButtonElement.id = 'playButton';
			playButtonElement.textContent = 'Play Stream';
			playButtonElement.classList.add('hidden');
			playButtonElement.addEventListener("click", playStream);

			statusDisplayElement = document.createElement('div');
			statusDisplayElement.id = 'status-display';
			statusDisplayElement.className = 'status-bar';
			statusDisplayElement.textContent = 'Connecting...';

			// Editable: the overlay hosts IME composition, and no browser activates an IME on a
			// read-only input.
			let overlayInput = document.createElement('input');
			overlayInput.type = 'search';
			overlayInput.readOnly = false;
			overlayInput.autocomplete = 'off';
			// Keeps a mobile soft keyboard off the taps this overlay collects for
			// the stream; #keyboard-input-assist is what deliberately opens one.
			overlayInput.inputMode = 'none';
			overlayInput.virtualKeyboardPolicy = 'manual';
			overlayInput.setAttribute('autocorrect', 'off');
			overlayInput.setAttribute('autocapitalize', 'off');
			overlayInput.setAttribute('spellcheck', 'false');
			overlayInput.id = 'overlayInput';

			videoElement = document.createElement('video');
			videoElement.id = 'stream';
			videoElement.className = 'video';
			videoElement.autoplay = true;
			videoElement.playsInline = true;
			videoElement.addEventListener('resize', () => {
				// The track's intrinsic size is the realized resolution; a divergence from the
				// request routes input through the video's fitted box instead of window math.
				const vw = videoElement.videoWidth, vh = videoElement.videoHeight;
				if (vw > 0 && vh > 0 && lastRequestedStreamRes) {
					window.streamResolutionDiverged =
						(vw !== lastRequestedStreamRes[0] || vh !== lastRequestedStreamRes[1]);
				}
			});

			const hiddenFileInput = document.createElement('input');
			hiddenFileInput.type = 'file';
			hiddenFileInput.id = 'globalFileInput';
			hiddenFileInput.multiple = true;
			hiddenFileInput.style.display = 'none';
			document.body.appendChild(hiddenFileInput);
			hiddenFileInput.addEventListener('change', handleFileInputChange);

			videoContainer.appendChild(videoElement);
			videoContainer.appendChild(playButtonElement);
			videoContainer.appendChild(statusDisplayElement);
			videoContainer.appendChild(overlayInput);
			appDiv.appendChild(videoContainer);

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
			appName = "webrtc"
			debug = getBoolParam('debug', false);
			turnSwitch = getBoolParam('turn_switch', false);
			resizeRemote = getBoolParam('resize_remote', resizeRemote);
			scaleLocal = getBoolParam('scaleLocallyManual', !resizeRemote);
			videoBitRate = getIntParam('video_bitrate', videoBitRate);
			videoFramerate = getIntParam('framerate', videoFramerate);
			audioBitRate = getIntParam('audio_bitrate', audioBitRate);
			window.manualResolution = getBoolParam('manual_resolution', false);
			isGamepadEnabled = getBoolParam('isGamepadEnabled', true);
			manualWidth = getIntParam('manual_width', null);
			manualHeight = getIntParam('manual_height', null);
			encoder = getStringParam('encoder', 'h264enc');
			rateControlMode = getStringParam('rate_control_mode', 'cbr');
			useCssScaling = getBoolParam('useCssScaling', false);
			scalingDPI = (getStringParam('scaling_dpi', null) !== null) ? getIntParam('scaling_dpi', 96) : autoDeriveDpi();
			enable_binary_clipboard = getBoolParam('enable_binary_clipboard', enable_binary_clipboard);
			clipboard_in_enabled = getBoolParam('clipboard_in_enabled', clipboard_in_enabled);
			clipboard_out_enabled = getBoolParam('clipboard_out_enabled', clipboard_out_enabled);
			crf = getIntParam('video_crf', crf);
			antiAliasingEnabled = getBoolParam('antiAliasingEnabled', true);
			trackpadMode = getBoolParam('trackpadMode', false);
			useBrowserCursors = getBoolParam('use_browser_cursors', true);
			force_aligned_resolution = getBoolParam('force_aligned_resolution', false);

			if (!isSharedMode) {
				window.addEventListener("message", handleMessage);
				window.addEventListener('requestFileUpload', handleRequestFileUpload);
				overlayInput.addEventListener('dragover', handleDragOver);
				overlayInput.addEventListener('drop', handleDrop);
			}
			// Applies to every page, viewers included.
			document.addEventListener('visibilitychange', handleVisibilityChange);

			const displayId = hash.startsWith('#display2') ? 'display2' : 'primary';
			let displayPosition = 'right';
			if (displayId === 'display2') {
				const posMatch = hash.match(/^#display2-(right|left|up|down)/);
				if (posMatch) displayPosition = posMatch[1];
			}
			/** Display rectangles (+ per-page scale) from the last display-config update. */
			let latestDisplayLayouts = null;

			var pathname = getRoutePrefix() + "/";
			var protocol = (location.protocol == "http:" ? "ws://" : "wss://");
			var url = new URL(protocol + window.location.host + pathname + "api/" + appName + "/signaling/");
			// Secure-mode token, matched against the active mk token to grant collaboration.
			var authToken = new URLSearchParams(window.location.search).get('token') || undefined;
			fatalConnectionHalt = false;
			let pcRecoveryTimer = null;
			var signaling = new WebRTCSignaling(url, clientRole, clientSlot, isStrictViewer, authToken, displayId, displayPosition);
			/**
			 * A plain GET on the signaling endpoint returns 409 exactly when the
			 * server is serving WebSockets: after repeated connect failures, probe
			 * once and converge the stored mode instead of reload-looping.
			 */
			signaling.onfatalretry = async () => {
				let flipGuard = null;
				try { flipGuard = sessionStorage.getItem('selkies_mode_flip'); } catch (e) { /* ignore */ }
				if (!flipGuard) {
					try {
						const probeURL = new URL(url.href);
						probeURL.protocol = (location.protocol === 'http:' ? 'http:' : 'https:');
						const res = await fetch(probeURL.href, { cache: 'no-store', headers: sessionAuthHeaders() });
						if (res.status === 409) {
							try { sessionStorage.setItem('selkies_mode_flip', '1'); } catch (e) { /* ignore */ }
							setStringParam('stream_mode', 'websockets');
							console.warn('[signaling] Server is serving WebSockets (endpoint 409); switching stored mode.');
						}
					} catch (e) { /* unreachable server: plain reload keeps retrying */ }
				}
				location.reload();
			};
			webrtc = new WebRTCClient(signaling, videoElement, 1);
			activeWebrtcClient = webrtc;
			/** Strict viewers send nothing until collaboration is granted. */
			const send = (data) => {
				if (isSharedMode && isStrictViewer && !collabInputGranted) return;
				webrtc.sendDataChannelMessage(data);
			}
			input = new Input(overlayInput, send, isSharedMode, playerInputTargetIndex, useCssScaling);
			input.setDisplayLayouts(latestDisplayLayouts, displayId);
			/**
			 * Assigned before attach(), so the pad resync inside it and a pad
			 * pressed before the channel opens go through the persisted toggle
			 * like any later connect. A disconnect only updates the reported
			 * state: the manager polls every slot, so the other pads keep working.
			 */
			input.ongamepadconnected = (gamepad_id) => {
				const connected = toggleGamepadConnection();
				if (connected) {
					gamepad.gamepadState = "connected";
					gamepad.gamepadName = gamepad_id;
					webrtc._setStatus('Gamepad connected: ' + gamepad_id);
				}
			};
			input.ongamepaddisconnected = () => {
				gamepad.gamepadState = "disconnected";
				gamepad.gamepadName = "none";
				webrtc._setStatus('Gamepad disconnected');
			};
			if (!isSharedMode) {
				window.addEventListener('focus', handleWindowFocus);
				window.addEventListener('blur', handleWindowBlur);
				// Registered before input attaches (both capture on window), so the paste-ordering
				// hold sees a Ctrl+V first.
				clipboardGestures.wire();
			}
			// Bound before negotiation ends: the overlay hosts IME composition from the start, and
			// sends into a closed channel drop quietly.
			input.attach();
			/**
			 * Window size in CSS pixels: the library default multiplies by
			 * devicePixelRatio, and every caller here applies the dpr itself, so
			 * HiDPI sessions would otherwise double-multiply.
			 */
			input.getWindowResolution = () => {
				const container = videoElement && videoElement.parentElement;
				if (!container) return [window.innerWidth, window.innerHeight];
				const rect = container.getBoundingClientRect();
				return [rect.width, rect.height];
			};
			window.webrtcInput = input;

			if (trackpadMode) input.setTrackpadMode(true);
			applyEffectiveCursorSetting();
			window.postMessage({ type: 'trackpadModeUpdate', enabled: trackpadMode }, window.location.origin);
			window.postMessage({ type: 'clientRoleUpdate', role: clientRole }, window.location.origin);

			setupKeyBoardAssisstant();

			signaling.onstatus = (message) => {
				pushCapped(logEntries, applyTimestamp("[signaling] " + message));
				console.log("[signaling] " + message);
			};
			signaling.onerror = (message) => {
				pushCapped(logEntries, applyTimestamp("[signaling] [ERROR] " + message))
				console.log("[signaling ERROR] " + message);
			};

			signaling.ondisconnect = (reconnect) => {
				videoElement.style.cursor = "auto";
				releaseWakeLock();
				if (window.__selkiesAuthProbe) window.__selkiesAuthProbe();
				if (reconnect) {
					status = 'connecting';
					webrtc.reset();
				} else {
					status = 'disconnected';
				}
				updateStatusDisplay();
			};

			/**
			 * A fatal server verdict (invalid slot, superseded takeover): stay
			 * down. The peer connection goes `failed` shortly after, and the
			 * recovery timer must not reload into an eviction ping-pong. The
			 * alert is suppressed during a mode switch, which closes the peer
			 * (code 4000) before the page reloads.
			 */
			signaling.onshowalert = (msg) => {
				fatalConnectionHalt = true;
				if (typeof window !== 'undefined' && window.__selkiesModeSwitching) return;
				alert("Disconnected: " + msg + " Please try again.");
			}

			webrtc.onstatus = (message) => {
				pushCapped(logEntries, applyTimestamp("[webrtc] " + message));
				console.log("[webrtc] " + message);
			};
			webrtc.onerror = (message) => {
				pushCapped(logEntries, applyTimestamp("[webrtc] [ERROR] " + message));
				console.log("[webrtc] [ERROR] " + message);
			};

			if (debug) {
				signaling.ondebug = (message) => { pushCapped(debugEntries, "[signaling] " + message); };
				webrtc.ondebug = (message) => { pushCapped(debugEntries, applyTimestamp("[webrtc] " + message)) };
			}

			webrtc.ongpustats = (stats) => {
				window.gpu_stats = stats;
			}

			/**
			 * Once the server tears the pipeline down only a fresh SDP exchange
			 * brings the picture back, so `failed` and `disconnected` reload to
			 * reconnect after a grace: `disconnected` can self-heal and gets the
			 * longer one, `failed` is final.
			 */
			webrtc.onconnectionstatechange = (state) => {
				videoConnected = state;
				if (videoConnected === "connected") {
					status = state;
					try { sessionStorage.removeItem('selkies_mode_flip'); } catch (e) { /* ignore */ }
					if (pcRecoveryTimer !== null) {
						clearTimeout(pcRecoveryTimer);
						pcRecoveryTimer = null;
					}
					if (!statWatchEnabled) {
						enableStatWatch();
					}
					requestWakeLock();
					applyOutputDevice();
				} else if (state === "failed" || state === "disconnected") {
					if (!fatalConnectionHalt && pcRecoveryTimer === null) {
						const graceMs = state === "failed" ? 1500 : 8000;
						pcRecoveryTimer = setTimeout(() => {
							pcRecoveryTimer = null;
							const st = webrtc.peerConnection && webrtc.peerConnection.connectionState;
							if (st === "connected" || fatalConnectionHalt) return;
							if (typeof window !== 'undefined' && window.__selkiesModeSwitching) return;
							console.warn(`[webrtc] connection ${st}; reloading to reconnect.`);
							location.reload();
						}, graceMs);
					}
				}
				updateStatusDisplay();
			};

			/**
			 * Pulls the server clipboard once on connect (cache-only; the server
			 * drops a viewer's `cr`), then restores the session settings and
			 * starts the client metrics loop. Input is bound for the page
			 * lifetime, so a reopened channel needs no rebind.
			 */
			webrtc.ondatachannelopen = () => {
				console.log("Data channel opened");
				try {
					taggedClipboardFetch.armLegacyWindow(5000);
					webrtc.sendDataChannelMessage('cr');
				} catch (e) {
					console.warn('Failed to send initial clipboard request (cr):', e);
				}
				// Input attaches before negotiation ends, so the pad announcement
				// it made went into a channel that was not open yet. Every role
				// re-announces: a #playerN link carries a gamepad and nothing else.
				if (input) input.resyncGamepads();

				if (isSharedMode) {
					console.log('Shared mode: skipping loading of last session settings and sending persisted settings to server');
					return;
				}

				loadLastSessionSettings();
				settleFullColorSupport().then(sendClientPersistedSettings);

				// One loop per channel: a reopened channel restarts it.
				if (metricsLoopId !== null) clearInterval(metricsLoopId);
				metricsLoopId = setInterval(async () => {
					if (connectionStat.connectionFrameRate === parseInt(connectionStat.connectionFrameRate, 10)) {
						webrtc.sendDataChannelMessage(`_f,${connectionStat.connectionFrameRate}`);
					}
					if (connectionStat.connectionLatency === parseInt(connectionStat.connectionLatency, 10)) {
						webrtc.sendDataChannelMessage(`_l,${connectionStat.connectionLatency}`);
					}
				}, 5000)
			}

			/** The core owns the hotkey chords; dashboards react to the posted messages. */
			input.onmenuhotkey = () => {
				showDrawer = !showDrawer;
				window.postMessage({ type: 'toggleDashboard' }, window.location.origin);
			}
			input.ongamepadhotkey = () => {
				window.postMessage({ type: 'toggleTouchGamepad' }, window.location.origin);
			}
			input.gamingMode = gamingModeActive;
			input.ongamingmode = (active) => {
				gamingModeActive = active;
				window.postMessage({ type: 'gamingModeUpdate', active }, window.location.origin);
			}

			webrtc.onplaystreamrequired = () => {
				showStart = true;
			}

			/**
			 * Caches server clipboard content and writes it locally when policy
			 * allows. A tagging server marks the payload answering this client's
			 * own `cr` with `reply_to`, which retires the timed heuristic for the
			 * session; it is armed before the shared-mode return so the state is
			 * consistent either way. Caching is unconditional, since gating it on
			 * clipboardStatus made the first payload depend on message ordering;
			 * only the local write is gated, on enablement, direction policy and
			 * the connect-time reply being cache-only. The fetch flag is consumed
			 * before the decode, so arrival order decides which payload settles
			 * the init fetch.
			 */
			webrtc.onclipboardcontent = async (msg) => {
				if (msg.data && msg.data.reply_to === 'cr') armTaggedClipboardReply();
				if (isSharedMode) {
					return;
				}
				const isInitClipboardFetch = consumeInitClipboardFetch();
				const {isMultipart, mimeType, content} = await handleClipboardData(msg);
				const isText = mimeType === "text/plain";
				if (isMultipart || content === null) {
					return;
				}
				const canWriteLocal = !isInitClipboardFetch &&
					clipboardStatus === 'enabled' && clipboard_out_enabled;

				if (isText) {
					// Freshness is computed before resolveServer records the signature.
					const isFreshContent = clipboardSync.shouldSend(content, 'text/plain');
					clipboardSync.resolveServer(content, null, 'text/plain');
					window.postMessage(clipboardPreviewMessage(content),
						window.location.origin);
					if (canWriteLocal && isFreshContent) {
						deferredClipboardWriter.write(
							() => navigator.clipboard.writeText(content), {
								onSuccess: () => console.log('Successfully wrote text from server to local clipboard.'),
								onFailure: (err) => console.log('Could not copy text to clipboard: ', err),
							});
					}
				} else if (enable_binary_clipboard) {
					let isFreshImage = true;
					try {
						const b = await content.getType(mimeType);
						const { byteLength, hash } = await clipboardWorker.hashBytes(await b.arrayBuffer());
						const digest = digestedPayload(byteLength, hash);
						isFreshImage = clipboardSync.shouldSend(digest, mimeType);
						clipboardSync.resolveServer(undefined, b, mimeType, digest);
					} catch (_) {}
					if (canWriteLocal && isFreshImage) {
						deferredClipboardWriter.write(
							() => navigator.clipboard.write([content]), {
								onSuccess: () => {
									console.log(`Successfully wrote image (${mimeType}) from server to local clipboard.`);
									clipboardSync.captureLocalImageSig();
									window.postMessage({
										type: 'clipboardContentUpdate',
										text: `Image (${mimeType}) received from session and copied to clipboard.`,
									}, window.location.origin);
								},
								onFailure: notifyClipboardImageWriteFailed,
							});
					} else if (isFreshImage && !isInitClipboardFetch && clipboard_out_enabled) {
						// Everything but the browser allows the write, so this is
						// a page with no clipboard to write to. An image has no
						// other way of showing up, and silence reads as the
						// session never having sent one.
						notifyClipboardImageWriteFailed(new Error('the local clipboard is unavailable'));
					}
				}
			}

			webrtc.oncursorchange = (cursorData) => {
				input.updateServerCursor(cursorData);
			}

			/** A secondary joining or leaving flips the multi-monitor cursor override. */
			webrtc.ondisplayconfig = (config) => {
				const displays = (config && config.displays) || [];
				latestDisplayLayouts = (config && config.layouts) || null;
				if (input && input.setDisplayLayouts) {
					input.setDisplayLayouts(latestDisplayLayouts, displayId);
				}
				const secondaryConnected = displays.some((d) => d !== 'primary');
				if (isSecondaryDisplayConnected !== secondaryConnected) {
					console.log(`Secondary display connection status changed to: ${secondaryConnected}`);
					isSecondaryDisplayConnected = secondaryConnected;
					applyEffectiveCursorSetting();
				}
			}

			/**
			 * Handles a server system action; the module docblock lists them. The
			 * role verdict overrides the hash-derived role and gamepad slot, which
			 * are only defaults. `resolution` reports what the server realized
			 * (snapped or clamped), which manual-mode bookkeeping follows so the UI
			 * stops re-requesting a size the server cannot produce.
			 */
			webrtc.onsystemaction = (action) => {
				webrtc._setStatus("Executing system action: " + action);
				if (action === 'reload') {
					setTimeout(() => {
						signaling.disconnect();
					}, 700);
				} else if (action.startsWith('mk_access,')) {
					const granted = action.slice('mk_access,'.length) === '1';
					collabInputGranted = granted;
					if (input) {
						if (granted) {
							if (!input.isInputAttached()) {
								console.log('Collab access granted: attaching input context.');
								input.attach_context();
							}
						} else {
							console.log('Collab access revoked: detaching input context.');
							input.detach_context();
						}
					}
				} else if (action.startsWith('command_error,') && !isSharedMode) {
					// Uses the fileUpload warning channel, or the optimistic UI reads as success.
					window.postMessage({
						type: 'fileUpload',
						payload: {
							status: 'warning',
							fileName: 'command',
							message: action.slice('command_error,'.length),
							code: 'commandFailed',
						},
					}, window.location.origin);
				} else if (action.startsWith('command_done,') && !isSharedMode) {
					// Settles what the apps panel shows as running; a failure
					// arrives on the channel above instead.
					window.postMessage({
						type: 'commandDone',
						command: action.slice('command_done,'.length),
					}, window.location.origin);
				} else if (action.startsWith('auth_success,') || action.startsWith('role_update,')) {
					const verdict = action.slice(action.indexOf(',') + 1);
					let perms;
					try {
						perms = JSON.parse(verdict);
					} catch (e) {
						console.error('Failed to parse role verdict:', e);
						return;
					}
					const isLiveRoleChange = action.startsWith('role_update,');
					const previousSlot = clientSlot;
					clientRole = perms.role === CLIENT_CONTROLLER ? CLIENT_CONTROLLER : CLIENT_VIEWER;
					clientSlot = (perms.slot === null || perms.slot === undefined) ? null : perms.slot;
					playerInputTargetIndex = (clientSlot !== null && clientSlot > 0) ? clientSlot - 1 : undefined;
					console.log(`Server role verdict: role=${clientRole}, slot=${clientSlot}`);
					if (input) {
						input.updateControllerSlot(clientSlot);
						if (clientRole === CLIENT_VIEWER) input.setSharedMode(true);
						// Only a live slot change gates polling: the initial verdict carries slot
						// null outside secure mode, meaning unmanaged, not revoked.
						if (isLiveRoleChange && input.gamepadManager) {
							if (previousSlot !== null && clientSlot === null) {
								input.gamepadManager.disable();
							} else if (previousSlot === null && clientSlot !== null && isGamepadEnabled) {
								input.gamepadManager.enable();
							}
						}
					}
					window.postMessage({ type: 'clientRoleUpdate', role: clientRole }, window.location.origin);
				} else if (action.startsWith('resolution,')) {
					const dims = action.slice('resolution,'.length).split('x');
					const rw = parseInt(dims[0], 10);
					const rh = parseInt(dims[1], 10);
					if (rw > 0 && rh > 0 && window.manualResolution &&
						(manualWidth !== rw || manualHeight !== rh)) {
						manualWidth = rw;
						manualHeight = rh;
						setIntParam('manual_width', rw);
						setIntParam('manual_height', rh);
						applyManualStyle(manualWidth, manualHeight, scaleLocal);
					}
				} else {
					webrtc._setStatus('Server sent acknowledgement for ' + action);
				}
			}

			webrtc.onlatencymeasurement = (latency_ms) => {
				serverLatency = latency_ms * 2.0;
			}

			webrtc.onsystemstats = (stats) => {
				window.system_stats = stats;
			}

			/**
			 * Applies the server settings payload: sanitizes the stored overrides,
			 * mirrors the policy gates (`command_enabled`, `enable_resize`, the
			 * clipboard directions, `enable_binary_clipboard`, which the stored
			 * choice governs unless locked), pushes the pre-copied local clipboard
			 * once the gates are in place, and switches between the manual and
			 * auto resize handlers.
			 */
			webrtc.onserversettings = (obj) => {
				if (obj.settings === undefined || obj.settings === null) {
					console.warn("Received invalid server settings paylod");
					return;
				}
				console.log("Received server settings payload:", obj.settings);
				const changes = sanitizeAndStoreSettings(obj.settings);
				const ce = obj.settings && obj.settings.command_enabled;
				serverCommandEnabled = (ce && typeof ce.value === 'boolean') ? ce.value : true;
				const er = obj.settings && obj.settings.enable_resize;
				if (er && typeof er.value === 'boolean') window.enable_resize = er.value;
				const cin = obj.settings && obj.settings.clipboard_in_enabled;
				if (cin && typeof cin.value === 'boolean') clipboard_in_enabled = cin.value;
				const cout = obj.settings && obj.settings.clipboard_out_enabled;
				if (cout && typeof cout.value === 'boolean') clipboard_out_enabled = cout.value;
				const ebc = obj.settings && obj.settings.enable_binary_clipboard;
				if (ebc && typeof ebc.value === 'boolean') {
					enable_binary_clipboard = ebc.locked ? ebc.value : getBoolParam('enable_binary_clipboard', ebc.value);
				}
				maybeSendInitialClipboard();
				window.postMessage({ type: 'serverSettings', payload: obj.settings }, window.location.origin);
				if (Object.keys(changes).length > 0) {
					console.log('Client settings were sanitized by server rules. Sending updates back to server:', changes);
					handleSettingsMessage(changes, true);
				}
				if (obj.settings.manual_resolution && obj.settings.manual_resolution.value === true) {
					console.log("Server settings payload confirms manual mode. Switching to manual resize handlers.");
					const serverWidth = obj.settings.manual_width ? parseInt(obj.settings.manual_width.value, 10) : 0;
					const serverHeight = obj.settings.manual_height ? parseInt(obj.settings.manual_height.value, 10) : 0;
					if (serverWidth > 0 && serverHeight > 0) {
						console.log(`Applying server-enforced manual resolution: ${serverWidth}x${serverHeight}`);
						window.manualResolution = true;
						manualWidth = serverWidth;
						manualHeight = serverHeight;
						applyManualStyle(manualWidth, manualHeight, scaleLocal);
					} else {
						console.warn("Server dictated manual mode but did not provide valid dimensions.");
					}
					disableAutoResize();
				} else {
					if (isSharedMode) {
						console.log("Shared mode detected, skipping auto resize enablement.");
						return;
					}
					console.log("Server settings payload confirms auto mode. Switching to auto resize handlers.");
					enableAutoResize();
				}

				if (obj.settings.enable_webrtc_statistics && obj.settings.enable_webrtc_statistics.value === true) {
					enableWebrtcStatics = true;
				}
			}

			// No permission query: Firefox and WebKit reject it, and Chromium reports 'prompt' until
			// persistent access is granted; each read handles its own errors.
			if (window.isSecureContext && navigator.clipboard) {
				clipboardStatus = 'enabled';
			}

			/**
			 * Applies an RTC configuration and opens the connection. Shared by
			 * the fetched and the fallback configuration, so a failed TURN fetch
			 * still connects: the data channel delivers the server settings, and
			 * without it the dashboard never renders its controls or the
			 * transport toggle.
			 */
			const applyRtcConfigAndConnect = (config) => {
				webrtc.forceTurn = turnSwitch;

				windowResolution = input.getWindowResolution();
				signaling.currRes = windowResolution;

				if (scaleLocal === false) {
						// Already CSS pixels; dividing by devicePixelRatio again would leave the
						// element too small until the first restyle.
						webrtc.element.style.width = windowResolution[0]+'px';
						webrtc.element.style.height = windowResolution[1]+'px';
				}

				if (config.iceServers && config.iceServers.length > 1) {
						pushCapped(debugEntries, applyTimestamp("using TURN servers: " + config.iceServers[1].urls.join(", ")));
				} else {
						pushCapped(debugEntries, applyTimestamp("no TURN servers found."));
				}
				webrtc.rtcPeerConfig = config;
				webrtc.connect();
			};

			fetch(getRoutePrefix() + "/api/turn", { headers: sessionAuthHeaders() })
				.then(function (response) {
					if (!response.ok) {
						throw new Error(`Status: ${response.status}`);
					}
					return response.json();
				})
				.then((config) => {
					applyRtcConfigAndConnect(config);
				})
				.catch((error) => {
					// A 404 is expected with no TURN server configured; host and STUN candidates
					// still serve LANs.
					pushCapped(debugEntries, applyTimestamp(`TURN config unavailable (${error}); connecting without TURN.`));
					console.warn(`Failed to fetch TURN server details (${error}); continuing without TURN.`);
					applyRtcConfigAndConnect({ iceServers: [] });
				})
		},
		/** Tears the session down: listeners, timers, the worker, and every session-scoped value back to its default. */
		cleanup() {
			window.manualResolution = false;
			window.fps = 0;
			stopWebcamCapture();

			window.removeEventListener("message", handleMessage);
			window.removeEventListener("resize", resizeStart);
			window.removeEventListener("requestFileUpload", handleRequestFileUpload);
			window.removeEventListener("focus", handleWindowFocus);
			window.removeEventListener("blur", handleWindowBlur);
			document.removeEventListener('visibilitychange', handleVisibilityChange);
			releaseWakeLock();
			preferredOutputDeviceId = null;
			clipboardGestures.unwire();

			try {
				clipboardWorker.terminate();
			} catch (error) {
				if (error.name === 'AbortError') return;
				console.error(error);
			}
			clipboardWorker = null;

			appName = null;
			videoBitRate = 8000;
			videoFramerate = 60;
			audioBitRate = 128000;
			showStart = false;
			showDrawer = false;
			logEntries = [];
			debugEntries = [];
			status = 'connecting';
			clipboardStatus = 'disabled';
			windowResolution = [];
			encoderLabel = "";
			encoder = ""
			gamepad = {
					gamepadState: 'disconnected',
					gamepadName: 'none',
			};
			connectionStat = {
					connectionStatType: "unknown",
					connectionLatency: 0,
					connectionVideoLatency: 0,
					connectionAudioLatency: 0,
					connectionAudioCodecName: "NA",
					connectionAudioBitrate: 0,
					connectionPacketsReceived: 0,
					connectionPacketsLost: 0,
					connectionBytesReceived: 0,
					connectionBytesSent: 0,
					connectionCodec: "unknown",
					connectionVideoDecoder: "unknown",
					connectionResolution: "",
					connectionFrameRate: 0,
					connectionVideoBitrate: 0,
					connectionAvailableBandwidth: 0
			};
			serverLatency = 0;
			resizeRemote = false;
			scaleLocal = false;
			debug = false;
			turnSwitch = false;
			playButtonElement = null;
			statusDisplayElement = null;
			rtime = null;
			rdelta = 500;
			rtimeout = false;
			manualWidth = 0, manualHeight = 0;
			isGamepadEnabled = true;
			videoConnected = "";
			audioConnected = "";
			statWatchEnabled = false;
			if (statsLoopId !== null) { clearInterval(statsLoopId); statsLoopId = null; }
			if (metricsLoopId !== null) { clearInterval(metricsLoopId); metricsLoopId = null; }
			clearResumeWatchdog();
			webrtc = null;
			activeWebrtcClient = null;
			input = null;
			useCssScaling = false;
			detectedSharedModeType = null;
			playerInputTargetIndex = 0;
			enableWebrtcStatics = false;
			enable_binary_clipboard = true;
			serverCommandEnabled = true;
			multipartClipboard.reset();

		}
	}
}
