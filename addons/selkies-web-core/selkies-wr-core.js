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

import { WebRTCClient } from "./lib/webrtc";
import { WebRTCSignaling } from "./lib/signaling";
import { Input } from "./lib/input";
import { createClipboardSync, createClipboardGestures, createDeferredClipboardWriter, clipboardPreviewMessage, readLocalClipboard } from "./lib/clipboard-sync.js";
import { createFileUploader } from "./lib/file-upload.js";
// Inline (base64 blob) so the worker travels inside selkies-core.js itself —
// no separate hashed file to place next to whichever chunk references it.
import { ClipboardWorkerBridge, sendClipboardChunked } from './lib/clipboard-worker-bridge.js'

// Per-transfer id so concurrent multipart clipboard sends are not interleaved.
let __clipboardTransferCounter = 0;
// Mirrors the server's command_enabled; default true for older servers that don't advertise it.
let serverCommandEnabled = true;

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

export default function webrtc() {
	let appName;
	let crf = 23;
	let videoBitRate = 8;      // in mbps
	let videoFramerate = 60;
	let audioBitRate = 128000; // in kbps
	let showStart = false;
	let showDrawer = false;
	// Log/debug entries are retained in capped ring buffers (devtools inspection via
	// window.selkiesLogs); everything is also mirrored to the console as it happens.
	const MAX_LOG_ENTRIES = 1000; // cap so the buffers can't grow for the whole session
	const pushCapped = (arr, v) => { arr.push(v); if (arr.length > MAX_LOG_ENTRIES) arr.shift(); };
	let logEntries = [];
	let debugEntries = [];
	window.selkiesLogs = { log: logEntries, debug: debugEntries };
	let status = 'connecting';
	let clipboardStatus = 'disabled';
	// Per-direction gates (server-synced): in = client->server, out = server->client.
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
	// Last stream resolution asked of the server, in physical stream pixels;
	// compared against the track's intrinsic size to detect a realized size
	// that differs from the request (mode snapping / rejected resize).
	var lastRequestedStreamRes = null;
	// Screen Wake Lock sentinel + preferred audio output device (parity with the WS core).
	let wakeLockSentinel = null;
	let preferredOutputDeviceId = null;
	let serverLatency = 0;
	let resizeRemote = false;
	let scaleLocal = false;
	let debug = false;
	let turnSwitch = false;
	let playButtonElement = null;
	let statusDisplayElement = null;
	let rtime = null;
	let rdelta = 500; // time in milliseconds
	let rtimeout = false;
	let manualWidth, manualHeight = 0;
	window.isManualResolutionMode = false;
	window.fps = 0;
	window.currentAudioBufferSize = 0;
	let enableWebrtcStatics = false;

	var videoConnected = "";
	var audioConnected = "";
	var statWatchEnabled = false;
	var webrtc = null;
	var input = null;
	// track interval ids so they can be cleared on cleanup/reconnect (avoid leaks/double-start)
	let statsLoopId = null;
	let metricsLoopId = null;
	let useCssScaling = false;
	// scaling_dpi (the desktop-DPI slider, 96 = 100%). INDEPENDENT of resolution / the HiDPI
	// toggle. Defaults to the local display scaling (devicePixelRatio) so the remote desktop's
	// fonts/UI match the local environment regardless of the streamed resolution; an explicit
	// slider value wins.
	let scalingDPI = 96;
	// Webrtc mode has video and audio active by default,
	// and no microphone support yet.
	let isVideoPipelineActive = true;
	let isAudioPipelineActive = true;
	let isMicrophoneActive = false;
	let isGamepadEnabled = true;

	// Per-message budget on the data channel: the browser exposes the negotiated
	// SCTP max-message-size (min of both ends); fall back to the 256 KiB standard
	// pre-negotiation, cap at 1 MiB to bound per-message buffering. The trailing
	// 512 bytes leave room for the message prefix/envelope.
	const dcMessageBudget = () => {
		const nego = (typeof webrtc !== 'undefined' && webrtc && webrtc.peerConnection &&
			webrtc.peerConnection.sctp && webrtc.peerConnection.sctp.maxMessageSize) || 0;
		const limit = nego > 0 ? Math.min(nego, 1024 * 1024) : 256 * 1024;
		return limit - 512;
	};
	const CLIENT_CONTROLLER = "controller";
	const CLIENT_VIEWER = "viewer";
	// leave some room for metadata in the message


	let detectedSharedModeType = null;
	let playerInputTargetIndex = 0;
	let clientRole = null;
	let clientSlot = null;

	// Render/input preferences shared with the websockets core (same
	// localStorage keys, same dashboard messages).
	let antiAliasingEnabled = true;
	let trackpadMode = false;
	let useBrowserCursors = false;
	// Whether a secondary display page is connected (server display_config_update
	// broadcast). Multi-monitor forces browser-cursor rendering: the server-drawn
	// cursor overlay only tracks one capture region.
	let isSecondaryDisplayConnected = false;
	// Round resolutions to multiples of 16 (encoder macroblock alignment) instead
	// of the default 2 when the force_aligned_resolution setting is on.
	let force_aligned_resolution = false;

	let enable_binary_clipboard = true;
	let multipartClipboard = {
		chunks: [],
		mimeType: '',
		totalSize: 0,
		inProgress: false
	};
	let clipboardWorker = new ClipboardWorkerBridge();
	let lastClipboardText = "";
	// Server-clipboard cache + change-only sync + Ctrl/Cmd+C request queue
	// (see lib/clipboard-sync.js). The send hook late-binds `webrtc`.
	const clipboardSync = createClipboardSync({
		sendRequest: () => webrtc.sendDataChannelMessage('REQUEST_CLIPBOARD')
	});
	// Server pushes carry no user activation; Firefox/WebKit reject the write
	// until the next real gesture, so those writes go through this retry queue.
	const deferredClipboardWriter = createDeferredClipboardWriter();
	const isChromium = (() => {
		const isIOS = /iPad|iPhone|iPod/.test(navigator.userAgent) ||
			(navigator.platform === 'MacIntel' && navigator.maxTouchPoints > 1);
		const isFirefox = /Firefox|FxiOS/.test(navigator.userAgent);
		const isCriOS = /CriOS/.test(navigator.userAgent);
		return typeof window.chrome !== 'undefined' && !isIOS && !isFirefox && !isCriOS;
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

	// Set storage key based on URL
	// Origin + pathname only (NOT the full URL): a per-session ?token=... must not mint
	// a new localStorage namespace each connect. Must match selkies-core.js / ws-core.
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

	// Per-display settings get a display2 suffix on the second-display page so
	// the two displays' picks never share (or clobber) one key. Must match the
	// dashboard's getPrefixedKey and the websockets core.
	const storageDisplayId = window.location.hash.startsWith('#display2') ? 'display2' : 'primary';
	const PER_DISPLAY_SETTINGS = [
		'framerate', 'video_crf', 'video_fullcolor',
		'video_streaming_mode', 'use_cpu',
		'video_paintover_crf', 'video_paintover_burst_frames', 'use_paint_over_quality',
		'is_manual_resolution_mode', 'manual_width', 'manual_height',
		'encoder_rtc', 'scaleLocallyManual', 'use_browser_cursors', 'rate_control_mode',
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
	// Fraction-preserving variant for values with sub-unit steps (Mbps bitrate).
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

	// Function to add timestamp to logs.
	var applyTimestamp = (msg) => {
		var now = new Date();
		var ts = now.getHours() + ":" + now.getMinutes() + ":" + now.getSeconds();
		return "[" + ts + "]" + " " + msg;
	}

	// Resolution rounding shared with the websockets core: 2-pixel alignment
	// normally (YUV 4:2:0 chroma), 16-pixel when force_aligned_resolution is on.
	const alignResolution = (num) => {
		const alignment = force_aligned_resolution ? 16 : 2;
		return Math.floor(num / alignment) * alignment;
	};

	// Browser-cursor rendering resolves from the user preference PLUS the
	// multi-monitor override (this page being a secondary, or the primary while a
	// secondary is connected) — the server-drawn cursor overlay only tracks one
	// capture region. Mirrors the websockets core.
	function applyEffectiveCursorSetting() {
		const userPreference = getBoolParam('use_browser_cursors', true);
		const isDisplay2 = window.location.hash.startsWith('#display2');
		const isMultiMonitorActive = (isDisplay2 || isSecondaryDisplayConnected);
		const finalSetting = isMultiMonitorActive ? true : userPreference;
		useBrowserCursors = finalSetting;
		if (input && typeof input.setUseBrowserCursors === 'function') {
			console.log(`Applying effective cursor setting. Multi-monitor: ${isMultiMonitorActive}, User Pref: ${userPreference}, Final: ${finalSetting}`);
			input.setUseBrowserCursors(finalSetting);
		}
		// Tell the dashboard the value actually in effect so its toggle reflects
		// the multi-monitor override instead of the user preference alone.
		try {
			window.postMessage({ type: 'effectiveCursorState', value: finalSetting }, window.location.origin);
		} catch (e) { /* postMessage unavailable */ }
	}

	function playStream() {
		showStart = false;
		if (playButtonElement) playButtonElement.classList.add('hidden');
		webrtc.playStream();
		requestWakeLock();
	}

	// Keep the screen awake while streaming. request() early-returns if already held
	// and no-ops (with a warning) where the API is absent.
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

	const releaseWakeLock = async () => {
		if (wakeLockSentinel !== null) {
			await wakeLockSentinel.release();
			wakeLockSentinel = null;
		}
	};

	// A backgrounded tab drops the wake lock automatically; re-acquire when visible.
	async function handleVisibilityChange() {
		if (!document.hidden && wakeLockSentinel === null) {
			await requestWakeLock();
		}
	}

	// Route WebRTC audio to a chosen output device. The <video> element carries both
	// audio and video (one bundled stream), so setSinkId on it moves the audio sink.
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

	function updateStatusDisplay() {
		if (statusDisplayElement) {
			// Sentence-case the status word for display (internal `status` stays lower-case
			// for comparisons like `status == 'connected'`): 'connecting' -> 'Connecting'.
			statusDisplayElement.textContent = status ? status.charAt(0).toUpperCase() + status.slice(1) : status;
			if (status == 'connected') {
				// clear the status and show the play button
				statusDisplayElement.classList.add("hidden");
				if (playButtonElement && showStart) {
					playButtonElement.classList.remove('hidden');
				}
			}
		}
	}

	function updateVideoImageRendering(){
		if (!videoElement) return;

		if (!antiAliasingEnabled) {
			// Same contract as the websockets core: anti-aliasing off forces
			// sharp pixels regardless of scaling.
			if (videoElement.style.imageRendering !== 'pixelated') {
				videoElement.style.imageRendering = 'pixelated';
			}
			return;
		}
		const dpr = window.devicePixelRatio || 1;
		const isOneToOne = !useCssScaling || (useCssScaling && dpr <= 1);
		if (isOneToOne) {
			// Use 'pixelated' for a sharp, 1:1 pixel look
			if (videoElement.style.imageRendering !== 'pixelated') {
				console.log("Setting video rendering to 'pixelated' for sharp display.");
				videoElement.style.imageRendering = 'pixelated';
			}
		} else {
			// Use 'auto' to let the browser smooth the upscaled video
			if (videoElement.style.imageRendering !== 'auto') {
				console.log("Setting video rendering to 'auto' for smooth upscaling.");
				videoElement.style.imageRendering = 'auto';
			}
		}
	};

	function sanitizeAndStoreSettings(serverSettings) {
		console.log("Sanitizing and storing settings based on server payload.");
		const changes = {};

		// Persist ONLY genuine user overrides. A server-pushed value with no stored
		// override is applied to the runtime (window[key]) but NOT written to
		// localStorage, so a later server-side change can still be re-pushed.
		// Persisting server defaults here left them stuck against future updates.
		for (const key in serverSettings) {
			if (!serverSettings.hasOwnProperty(key)) continue;
			const setting = serverSettings[key];
			const finalKey = storageKeyFor(key);
			const wasUnset = window.localStorage.getItem(finalKey) === null;

			if (setting.min !== undefined && setting.max !== undefined) {
				// Float-aware: fractional ranges (sub-Mbps bitrate) must not be
				// parsed as ints — that reads "0.5" as 0, flags it out of range,
				// and wipes the pick back to the server default on every connect.
				// In-range stored values are kept verbatim (no write-back).
				const clientValue = getFloatParam(key, setting.default);
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
					// Not persisted: the lock governs at runtime, and writing it into the
					// user's own key would masquerade as their pick after an unlock.
				} else if (wasUnset) {
					window[key] = serverValue;
					if (setting.overridden) {
						// An operator-configured (unlocked) value must actually be applied
						// when the user has no stored pick — mirroring window state alone
						// leaves runtime consumers on their built-in defaults.
						changes[key] = serverValue;
					}
				} else {
					const clientValue = getBoolParam(key, serverValue);
					window[key] = clientValue;
					setBoolParam(key, clientValue);
				}
			}
		}
		return changes;
	}

	function sendClientPersistedSettings() {
		if (isSharedMode) {
			console.log("Skipping sending client persisted settings in shared mode.");
			return;
		}
		// Every display page sends its persisted settings: the server applies a
		// payload to the display whose channel delivered it, so a secondary
		// configures only its own stream (websockets model). Its resolution
		// still flows through the standard resize message.
		const settingsPrefix = `${storageAppName}_`;
		const settingsToSend = {};
		const dpr = useCssScaling ? 1 : (window.devicePixelRatio || 1);

		const knownSettings = [
			'framerate', 'encoder_rtc', 'is_manual_resolution_mode',
			'audio_bitrate', 'video_bitrate', 'scaling_dpi', 'enable_binary_clipboard',
			'rate_control_mode', 'video_crf', 'use_cpu',
			'video_fullcolor', 'video_streaming_mode', 'use_paint_over_quality',
			'video_paintover_crf', 'video_paintover_burst_frames'
		];
		const booleanSettingKeys = [
			'is_manual_resolution_mode', 'enable_binary_clipboard', 'use_cpu',
			'video_fullcolor', 'video_streaming_mode', 'use_paint_over_quality'
		];
		const integerSettingKeys = [
			'framerate', 'audio_bitrate', 'scaling_dpi', 'video_crf',
			'video_paintover_crf', 'video_paintover_burst_frames'
		];
		// video_bitrate (Mbps) allows sub-Mbps fractions (0.25 = 250 Kbps); an
		// integer parse would truncate it to 0 on this initial settings send.
		const floatSettingKeys = ['video_bitrate'];

		for (const key in localStorage) {
			if (Object.hasOwnProperty.call(localStorage, key) && key.startsWith(settingsPrefix)) {
				const unprefixedKey = key.substring(settingsPrefix.length);
				// Per-display keys carry a display2 suffix: this display reads only
				// its own variant, and the primary skips display2's keys, so picks
				// never leak across displays.
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
					} else if (floatSettingKeys.includes(baseKey)) {
						value = parseFloat(value);
						if (isNaN(value)) continue;
					} else if (integerSettingKeys.includes(baseKey)) {
						value = parseInt(value, 10);
						if (isNaN(value)) continue;
					}
					settingsToSend[baseKey] = value;
				}
			}
		}

		if (window.isManualResolutionMode && manualWidth != null && manualHeight != null) {
			settingsToSend['is_manual_resolution_mode'] = true;
			// Manual dimensions are exact physical pixels by definition — no dpr
			// multiply (parity with the websockets core and this core's own
			// resize-message path, which both send them raw).
			settingsToSend['manual_width'] = alignResolution(manualWidth);
			settingsToSend['manual_height'] = alignResolution(manualHeight);
		}
		settingsToSend['useCssScaling'] = useCssScaling;

		try {
			const settingsJson = JSON.stringify(settingsToSend);
			webrtc.sendDataChannelMessage(`SETTINGS,${settingsJson}`);
			console.log('Sent initial settings to server:', settingsToSend);
		} catch (e) {
			console.error('Error constructing or sending initial settings:', e);
		}
	}

	function applyManualStyle(targetWidth, targetHeight, scaleToFit) {
		if (targetWidth <=0 || targetHeight <=0) {
			console.log("Invalid target height or width")
			return;
		}

		const dpr = (window.isManualResolutionMode || useCssScaling) ? 1 : (window.devicePixelRatio || 1);
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
			videoElement.style.objectFit = 'contain'; // Should be 'fill' if CSS handles aspect ratio
			console.log(`Applied manual style (Scaled): CSS ${cssWidth}x${cssHeight}, Pos ${leftOffset},${topOffset}`);
		} else {
			// Center the exact-size box too (ws-core parity): a viewport larger
			// than the stream otherwise leaves it pinned to the top-left corner.
			const topOffset = (containerHeight - targetHeight) / 2;
			const leftOffset = (containerWidth - targetWidth) / 2;
			videoElement.style.position = 'absolute';
			videoElement.style.width = `${targetWidth}px`;
			videoElement.style.height = `${targetHeight}px`;
			videoElement.style.top = `${topOffset}px`;
			videoElement.style.left = `${leftOffset}px`;
			videoElement.style.objectFit = 'fill'; // Use 'fill' to ignore aspect ratio
			console.log(`Applied manual style (Exact): CSS ${targetWidth}x${targetHeight}, Pos ${leftOffset},${topOffset}`);
		}
		updateVideoImageRendering();
	}

	function resetToWindowResolution(targetWidth, targetHeight) {
		if (!videoElement) return;

		// Buffer hint in physical pixels; the on-screen box stays at CSS pixels
		// (`target*`) — styling with physical pixels overflows the viewport by
		// dpr^2 on HiDPI displays.
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

	// scaling_dpi synced to the local display scaling (devicePixelRatio), NOT the resolution:
	// dpr 1.5 -> 144 (150%), 2 -> 192 (200%); 96 (100%) otherwise. Snapped to the DPI presets.
	function autoDeriveDpi() {
		const dpr = window.devicePixelRatio || 1;
		const target = Math.round(dpr * 4) * 24;
		return (dpr > 1 && [120, 144, 168, 192, 216, 240, 288].includes(target)) ? target : 96;
	}

	function sendResolutionToServer(width, height) {
		if (isSharedMode) {
			console.log("Skipping sending resolution in shared mode.");
			return;
		}
		let realWidth, realHeight, dpr;
		if (window.isManualResolutionMode) {
			// A manual/preset resolution IS the exact framebuffer; don't multiply by dpr, or a
			// useCssScaling flip (HiDPI toggle / preset apply) swings it 2x<->1x. Mirrors ws-core.
			dpr = 1;
			realWidth = alignResolution(width);
			realHeight = alignResolution(height);
		} else {
			dpr = useCssScaling ? 1 : (window.devicePixelRatio || 1);
			realWidth = alignResolution(width * dpr);
			realHeight = alignResolution(height * dpr);
		}
		const resString = `${realWidth}x${realHeight}`;
		lastRequestedStreamRes = [realWidth, realHeight];
		console.log(`Sending resolution to server: ${resString}, Pixel Ratio Used: ${dpr}, useCssScaling: ${useCssScaling}`);
		webrtc.sendDataChannelMessage(`r,${resString}`);
	}

	function enableAutoResize() {
		window.addEventListener("resize", resizeStart);
	}

	function disableAutoResize() {
		window.removeEventListener("resize", resizeStart);
	}

	// Manual-resolution mode detaches the auto-resize listener, but the manual
	// style's CENTERING offsets still depend on the container size: recompute
	// them when the window geometry changes (fullscreen enter/exit, window
	// resize) or the stream stays anchored where it was first placed.
	// Self-gating (no-op outside manual mode), so it is registered once.
	window.addEventListener('resize', () => {
		if (window.isManualResolutionMode && !isSharedMode
			&& manualWidth > 0 && manualHeight > 0 && videoElement && videoElement.parentElement) {
			applyManualStyle(manualWidth, manualHeight, scaleLocal);
		}
	});

	function resizeStart() {
		rtime = new Date();
		if (rtimeout === false) {
			rtimeout = true;
			setTimeout(() => { resizeEnd() }, rdelta);
		}
	}

	function resizeEnd() {
		if (new Date() - rtime < rdelta) {
			setTimeout(() => { resizeEnd() }, rdelta);
		} else {
			rtimeout = false;
			windowResolution = input.getWindowResolution();
			sendResolutionToServer(windowResolution[0], windowResolution[1])
			resetToWindowResolution(windowResolution[0], windowResolution[1])
		}
	}

	// Auto-mode framebuffer resolution is logical-size x devicePixelRatio, but a DPR
	// change alone (window dragged to a monitor of a different pixel density, or an OS
	// display-scaling change) fires no 'resize' event, so the stream stays at the old
	// density until the next resize. Re-run the auto-resize path when DPR changes;
	// self-gated to auto mode (mirrors the manual-centering resize listener above).
	// matchMedia resolution queries are one-shot at a given dppx, so re-arm each time.
	const watchDevicePixelRatio = () => {
		let mql = null;
		const onDprChange = () => {
			if (!window.isManualResolutionMode && !isSharedMode) { resizeStart(); }
			arm();
		};
		const arm = () => {
			if (mql) { try { mql.removeEventListener('change', onDprChange); } catch (_) {} }
			const dpr = window.devicePixelRatio || 1;
			mql = window.matchMedia(`(resolution: ${dpr}dppx)`);
			mql.addEventListener('change', onDprChange, { once: true });
		};
		arm();
	};
	watchDevicePixelRatio();

	function loadLastSessionSettings() {
		if (isSharedMode) {
			console.log("Skipping loading last session settings in shared mode.");
			return;
		}
		// Sync the remote desktop DPI to the local display scaling on connect (server applies via
		// handle_scaling -> set_dpi). scaling_dpi is not in the WebRTC settings allow-list, so the
		// s, path is required. This is the desktop-font sync, unrelated to the resolution.
		if (webrtc) { try { webrtc.sendDataChannelMessage(`s,${scalingDPI}`); } catch (_) {} }
		// Preset the video element to last session resolution
		if (window.isManualResolutionMode && manualWidth && manualHeight) {
			console.log(`Applying manual resolution: ${manualWidth}x${manualHeight}`);
			applyManualStyle(manualWidth, manualHeight, scaleLocal);
			// A secondary display lays out from its reported size, so a manual-mode
			// secondary must report on connect too; the auto branch below covers the primary.
			if (window.location.hash.startsWith('#display2')) {
				sendResolutionToServer(manualWidth, manualHeight);
			}
		} else {
			console.log("Applying window resolution");
			// If manual resolution is not set, reset to window resolution
			const currentWindowRes = input.getWindowResolution();
			resetToWindowResolution(...currentWindowRes);
			sendResolutionToServer(currentWindowRes[0], currentWindowRes[1]);
			enableAutoResize();
		}
	}

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

	// callback invoked when "message" event is triggerd
	function handleMessage(event) {
		if (event.origin !== window.location.origin) {
			console.warn("Received message from unexpected origin");
			return;
		}
		let message = event.data;
		switch(message.type) {
			case "setScaleLocally":
				if (typeof message.value === 'boolean') {
					console.log("Scaling the stream locally: ", message.value);
					// setScaleLocally returns true or false; false, to turn off the scaling
					if (message.value === true) disableAutoResize();
					scaleLocal = message.value;
					if (manualWidth && manualHeight) {
						applyManualStyle(manualWidth, manualHeight, scaleLocal);
						setBoolParam("scaleLocallyManual", scaleLocal);
					}
				} else {
					console.warn("Invalid value received for setScaleLocally:", message.value);
				}
				break;
			case "resetResolutionToWindow":
				console.log("Resetting to window size");
				manualHeight = manualWidth = 0; // clear manual W&H
				let currentWindowRes = input.getWindowResolution();
				resetToWindowResolution(...currentWindowRes);
				sendResolutionToServer(...currentWindowRes);
				enableAutoResize();
				// Use snake_case keys (read at init); the old camelCase keys were never read back.
				setIntParam('manual_width', null);
				setIntParam('manual_height', null);
				setBoolParam('is_manual_resolution_mode', false);
				window.isManualResolutionMode = false;
				break;
			case "setManualResolution":
				const width = parseInt(message.width, 10);
				const height = parseInt(message.height, 10);
				if (isNaN(width) || width <= 0 || isNaN(height) || height <= 0) {
					console.error('Received invalid width/height for setManualResolution:', message);
					break;
				}
				console.log(`Setting manual resolution: ${width}x${height}`);
				disableAutoResize();
				manualWidth = width;
				manualHeight = height;
				applyManualStyle(manualWidth, manualHeight, scaleLocal);
				sendResolutionToServer(manualWidth, manualHeight);
				// Use snake_case keys (read at init) so the choice persists across reloads.
				setIntParam('manual_width', manualWidth);
				setIntParam('manual_height', manualHeight);
				setBoolParam('is_manual_resolution_mode', true);
				window.isManualResolutionMode = true;
				break;
			case "setUseCssScaling":
				// ws-core parity. hiDPI is handled by re-deriving the DPR everywhere the
				// flag matters: sendResolutionToServer/resetToWindowResolution multiply by
				// devicePixelRatio only when CSS scaling is off, and input.updateCssScaling
				// realigns the coordinate math (touch included via the shared sink mapper).
				if (typeof message.value === 'boolean') {
					const changed = useCssScaling !== message.value;
					useCssScaling = message.value;
					setBoolParam('useCssScaling', useCssScaling);
					console.log(`Set useCssScaling to ${useCssScaling} and persisted.`);
					if (input && typeof input.updateCssScaling === 'function') {
						input.updateCssScaling(useCssScaling);
					}
					if (changed) {
						updateVideoImageRendering();
						if (window.isManualResolutionMode && manualWidth != null && manualHeight != null) {
							sendResolutionToServer(manualWidth, manualHeight);
							applyManualStyle(manualWidth, manualHeight, scaleLocal);
						} else if (!isSharedMode && input) {
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
				if (!serverCommandEnabled) {
					console.log("Command sending suppressed: server has command_enabled=false; not sending 'cmd,'.");
					break;
				}
				// && (not ||) so only a real value is forwarded, not the string "null"/"undefined".
				if (message.value !== null && message.value !== undefined) {
					const commandString = message.value;
					console.log(`Received 'command' message with value: "${commandString}"`);
					webrtc.sendDataChannelMessage(`cmd,${commandString}`);
				} else {
					console.warn(`Received invalid command from dashboard: ${message.value}`)
				}
				break;
			case 'pipelineControl':
				// The only pipeline the WebRTC client toggles is the microphone (video and
				// audio stay negotiated for the session); attach/detach the mic track.
				if (message.pipeline === 'microphone' && webrtc && typeof webrtc.setMicrophone === 'function') {
					const micOn = !!message.enabled;
					webrtc.setMicrophone(micOn).then(() => {
						isMicrophoneActive = micOn;
						postSidebarButtonUpdate();
					}).catch((e) => {
						console.error('Microphone toggle failed:', e);
						isMicrophoneActive = false;
						postSidebarButtonUpdate();
					});
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
				const newClipboardText = message.text;
				sendClipboardData(newClipboardText);
				break;
			case 'clipboardImageUpdate':
				// Dashboard image upload: hand the blob to the same binary path the
				// focus/paste read uses. Only meaningful when binary clipboard is on
				// (the server drops image writes otherwise).
				if (isSharedMode) {
					console.log("Shared mode: Clipboard image write to server blocked.");
					break;
				}
				if (message.imageBlob && enable_binary_clipboard) {
					(async () => {
						try {
							const buf = await message.imageBlob.arrayBuffer();
							await sendClipboardData(buf, message.imageBlob.type || 'image/png');
						} catch (e) {
							console.warn('Failed to send uploaded clipboard image:', e);
						}
					})();
				}
				break;
			case 'audioDeviceSelected':
				// Output-device routing (setSinkId); mic-input device selection is not
				// plumbed on the WebRTC mic path, so only 'output' is honored here.
				if (message.context === 'output' && message.deviceId) {
					preferredOutputDeviceId = message.deviceId;
					applyOutputDevice();
				}
				break;
			case 'requestFullscreen':
				// Parity with the websockets core: fullscreen the stream container
				// (pointer-lock aware) rather than the whole document.
				if (input) {
					input.enterFullscreen();
				} else if (document.fullscreenElement === null) {
					document.documentElement.requestFullscreen().catch(() => {});
				}
				break;
			case 'setSynth':
				if (input && typeof input.setSynth === 'function') {
					input.setSynth(message.value);
				}
				break;
			case 'showVirtualKeyboard': {
				// Parity with ws-core: focus the off-screen assist input so the
				// mobile soft keyboard opens; blur it on the next touch of the stream.
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
					setBoolParam('use_browser_cursors', message.value);
					// The multi-monitor override may force the effective value on.
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
				}
				break;
			case 'touchinput:touch':
				if (input && typeof input.setTrackpadMode === 'function') {
					trackpadMode = false;
					setBoolParam('trackpadMode', false);
					input.setTrackpadMode(false);
				}
				break;
			default:
				break;
		}
	}

	function handleSettingsMessage(settings) {
		// Turbo/4:4:4/paint-over have no dedicated data-channel opcode; the server applies
		// them via handle_update_settings, so forward them as a SETTINGS payload (mirrors the
		// WebSocket SETTINGS path; the dashboard already persisted them to localStorage).
		const passthrough = {};
		if (settings.video_fullcolor !== undefined) passthrough.video_fullcolor = !!settings.video_fullcolor;
		if (settings.video_streaming_mode !== undefined) passthrough.video_streaming_mode = !!settings.video_streaming_mode;
		if (settings.use_paint_over_quality !== undefined) passthrough.use_paint_over_quality = !!settings.use_paint_over_quality;
		if (settings.video_paintover_crf !== undefined) passthrough.video_paintover_crf = parseInt(settings.video_paintover_crf, 10);
		if (settings.video_paintover_burst_frames !== undefined) passthrough.video_paintover_burst_frames = parseInt(settings.video_paintover_burst_frames, 10);
		if (settings.force_aligned_resolution !== undefined) passthrough.force_aligned_resolution = !!settings.force_aligned_resolution;
		if (settings.use_cpu !== undefined) passthrough.use_cpu = !!settings.use_cpu;
		// Encoder switch (h264enc <-> openh264enc): the server restarts the pipeline on this.
		if (settings.encoder_rtc !== undefined) passthrough.encoder_rtc = settings.encoder_rtc;
		if (Object.keys(passthrough).length > 0) {
			webrtc.sendDataChannelMessage(`SETTINGS,${JSON.stringify(passthrough)}`);
		}
		if (settings.video_bitrate !== undefined) {
			videoBitRate = parseFloat(settings.video_bitrate);
			webrtc.sendDataChannelMessage(`vb,${videoBitRate}`);
			setIntParam('video_bitrate', videoBitRate);
		}
		if (settings.framerate !== undefined) {
			videoFramerate = parseInt(settings.framerate);
			webrtc.sendDataChannelMessage(`_arg_fps,${videoFramerate}`);
			setIntParam('framerate', videoFramerate);
		}
		if (settings.audio_bitrate !== undefined) {
			audioBitRate = parseInt(settings.audio_bitrate);
			webrtc.sendDataChannelMessage(`ab,${audioBitRate}`);
			setIntParam('audio_bitrate', audioBitRate);
		}
		if (settings.encoder_rtc !== undefined) {
			// The server restarts the pipeline with the new encoder (forwarded via the
			// SETTINGS passthrough above); track it locally for the decode path.
			encoder = settings.encoder_rtc;
			setStringParam('encoder_rtc', encoder);
			console.log("Encoder switched to:", encoder);
		}
		if (settings.scaling_dpi !== undefined) {
			const dpi = parseInt(settings.scaling_dpi, 10);
			if (!isNaN(dpi) && dpi > 0) {
				// Not persisted here: the localStorage pin belongs to the dashboard,
				// which writes it only for an explicit slider pick. Persisting every
				// posted value would re-pin the derived-default and reset-to-derived
				// posts, freezing DPI across displays with different devicePixelRatio
				// (the connect path derives the DPI when unpinned).
				scalingDPI = dpi;
				webrtc.sendDataChannelMessage(`s,${dpi}`);
			}
		}
		if (settings.enable_binary_clipboard !== undefined) {
			enable_binary_clipboard = !!settings.enable_binary_clipboard;
			webrtc.sendDataChannelMessage(`_ebc,${enable_binary_clipboard}`);
			setBoolParam('enable_binary_clipboard', enable_binary_clipboard);
			console.log(`Binary clipboard support ${enable_binary_clipboard ? 'enabled' : 'disabled'}`);
		}
		if (settings.clipboard_in_enabled !== undefined) {
			clipboard_in_enabled = !!settings.clipboard_in_enabled;
			setBoolParam('clipboard_in_enabled', clipboard_in_enabled);
		}
		if (settings.clipboard_out_enabled !== undefined) {
			clipboard_out_enabled = !!settings.clipboard_out_enabled;
			setBoolParam('clipboard_out_enabled', clipboard_out_enabled);
		}
		if (settings.rate_control_mode !== undefined) {
			rateControlMode = settings.rate_control_mode;
			webrtc.sendDataChannelMessage(`_rc,${rateControlMode}`);
			sendRespectiveRCvalue(rateControlMode);
			setStringParam('rate_control_mode', rateControlMode);
			console.log(`Rate control mode set to ${rateControlMode}`);
		}
		if (settings.video_crf !== undefined) {
			crf = parseInt(settings.video_crf, 10);
			webrtc.sendDataChannelMessage(`_crf,${crf}`);
			setIntParam('video_crf', crf);
			console.log(`H264 CRF set to ${crf}`);
		}
		if (settings.force_aligned_resolution !== undefined) {
			force_aligned_resolution = !!settings.force_aligned_resolution;
			setBoolParam('force_aligned_resolution', force_aligned_resolution);
			// Re-assert the current resolution so the stream snaps to the new
			// alignment without waiting for the next window resize.
			if (window.isManualResolutionMode && manualWidth != null && manualHeight != null) {
				sendResolutionToServer(manualWidth, manualHeight);
			} else if (!isSharedMode && input) {
				const currentWindowRes = input.getWindowResolution();
				sendResolutionToServer(currentWindowRes[0], currentWindowRes[1]);
			}
		}
	}

	function sendRespectiveRCvalue(newMode) {
		if (newMode === "cbr") {
			webrtc.sendDataChannelMessage(`vb,${videoBitRate}`);
		} else if (newMode === "crf") {
			webrtc.sendDataChannelMessage(`_crf,${crf}`);
		}
	};

	// HTTP uploads + drag-drop/file-picker plumbing live in the shared factory
	// (see lib/file-upload.js); shared sessions must not upload.
	const fileUploader = createFileUploader({ canUpload: () => !isSharedMode });
	const handleRequestFileUpload = fileUploader.handleRequestFileUpload;
	const handleFileInputChange = fileUploader.handleFileInputChange;
	const handleDragOver = fileUploader.handleDragOver;
	const handleDrop = fileUploader.handleDrop;

	// Metrics surfacing contract: the essentials are published on window (fps,
	// network_stats, video_bitrate) for the sidebar/dashboard bridge, the full
	// connectionStat object stays readable here, and enableWebrtcStatics optionally
	// streams the raw reports to the server as `_stats_video`.
	function enableStatWatch() {
		if (isSharedMode) {
			console.log("Shared mode detected, skipping stats watch setup.");
			return;
		}
		// Start watching stats
		var videoBytesReceivedStart = 0;
		var audioBytesReceivedStart = 0;
		var previousVideoJitterBufferDelay = 0.0;
		var previousVideoJitterBufferEmittedCount = 0;
		var previousAudioJitterBufferDelay = 0.0;
		var previousAudioJitterBufferEmittedCount = 0;
		var statsStart = new Date().getTime() / 1000;
		if (statsLoopId !== null) return; // already running; non-racy gate
		statWatchEnabled = true; // set synchronously before async work
		statsLoopId = setInterval(async () => {
			var now = new Date().getTime() / 1000;
			try {
				const stats = await webrtc.getConnectionStats();
				connectionStat = {};

				// Connection latency in milliseconds
				const rtt = (stats.general.currentRoundTripTime !== null) ? (stats.general.currentRoundTripTime * 1000.0) : (serverLatency)

				// Connection stats
				connectionStat.connectionPacketsReceived = stats.general.packetsReceived;
				connectionStat.connectionPacketsLost = stats.general.packetsLost;
				connectionStat.connectionStatType = stats.general.connectionType
				connectionStat.connectionBytesReceived = (stats.general.bytesReceived * 1e-6).toFixed(2) + " MBytes";
				connectionStat.connectionBytesSent = (stats.general.bytesSent * 1e-6).toFixed(2) + " MBytes";
				connectionStat.connectionAvailableBandwidth = (parseInt(stats.general.availableReceiveBandwidth) / 1e+6).toFixed(2) + " mbps";

				// Video stats
				connectionStat.connectionCodec = stats.video.codecName;
				connectionStat.connectionVideoDecoder = stats.video.decoder;
				connectionStat.connectionResolution = stats.video.frameWidth + "x" + stats.video.frameHeight;
				connectionStat.connectionFrameRate = stats.video.framesPerSecond;
				connectionStat.connectionVideoBitrate = (((stats.video.bytesReceived - videoBytesReceivedStart) / (now - statsStart)) * 8 / 1e+6).toFixed(2);
				videoBytesReceivedStart = stats.video.bytesReceived;

				// Audio stats
				connectionStat.connectionAudioCodecName = stats.audio.codecName;
				connectionStat.connectionAudioBitrate = (((stats.audio.bytesReceived - audioBytesReceivedStart) / (now - statsStart)) * 8 / 1e+3).toFixed(2);
				audioBytesReceivedStart = stats.audio.bytesReceived;
				// NetEQ concealment counters — the RED before/after acceptance metric.
				connectionStat.connectionAudioConcealedSamples = stats.audio.concealedSamples;
				connectionStat.connectionAudioConcealmentEvents = stats.audio.concealmentEvents;
				connectionStat.connectionAudioTotalSamplesReceived = stats.audio.totalSamplesReceived;
				connectionStat.connectionAudioPacketsDiscarded = stats.audio.packetsDiscarded;
				// Anchor the time window with the byte baselines (success path only) so the
				// next tick's byte window and time window cover the same interval.
				statsStart = now;

				// Latency stats
				connectionStat.connectionVideoLatency = parseInt(Math.round(rtt + (1000.0 * (stats.video.jitterBufferDelay - previousVideoJitterBufferDelay) / (stats.video.jitterBufferEmittedCount - previousVideoJitterBufferEmittedCount) || 0)));
				previousVideoJitterBufferDelay = stats.video.jitterBufferDelay;
				previousVideoJitterBufferEmittedCount = stats.video.jitterBufferEmittedCount;
				connectionStat.connectionAudioLatency = parseInt(Math.round(rtt + (1000.0 * (stats.audio.jitterBufferDelay - previousAudioJitterBufferDelay) / (stats.audio.jitterBufferEmittedCount - previousAudioJitterBufferEmittedCount) || 0)));
				// Audio-buffer proxy so the dashboard's Audio Buffer gauge works in WebRTC too:
				// the RTCInboundRtpStreamStats de-jitter depth (ms) over the ~20ms Opus frame is
				// roughly the number of frames buffered ahead of playout (browser-managed audio
				// has no direct frame count like the websockets worklet).
				const _audioJitterMs = 1000.0 * (stats.audio.jitterBufferDelay - previousAudioJitterBufferDelay) / (stats.audio.jitterBufferEmittedCount - previousAudioJitterBufferEmittedCount) || 0;
				window.currentAudioBufferSize = Math.max(0, Math.round(_audioJitterMs / 20));
				previousAudioJitterBufferDelay = stats.audio.jitterBufferDelay;
				previousAudioJitterBufferEmittedCount = stats.audio.jitterBufferEmittedCount;

				// Format latency
				connectionStat.connectionLatency =  Math.max(connectionStat.connectionVideoLatency, connectionStat.connectionAudioLatency);

				window.fps = connectionStat.connectionFrameRate;
				window.network_stats = {
					// Actual received throughput (video Mbps + audio kbps→Mbps), matching the WS
					// server-side bandwidth stat. availableReceiveBandwidth is only the
					// congestion-control estimate and reads far below the real rate on a relay.
					"bandwidth_mbps": (parseFloat(connectionStat.connectionVideoBitrate) || 0) + (parseFloat(connectionStat.connectionAudioBitrate) || 0) / 1000,
					"latency_ms": connectionStat.connectionLatency,
				};
				if (enableWebrtcStatics) webrtc.sendDataChannelMessage(`_stats_video,${JSON.stringify(stats.allReports)}`);
			} catch (e) {
				// webrtc may be null after cleanup; log anything unexpected for observability.
				// Don't re-anchor statsStart here: on error the byte baselines are NOT updated,
				// so advancing only the time window would inflate the next tick's bitrate.
				if (webrtc !== null) console.warn("Error collecting connection stats:", e);
			}
		// Stats refresh interval (1000 ms)
		}, 1000);
	}

	// Settles when the in-flight local-clipboard read+send completes; null when idle.
	let clipboardSendInFlight = null;

	async function readLocalClipboardAndSend() {
		if (!window.isSecureContext || isSharedMode || clipboardStatus !== "enabled" || !clipboard_in_enabled) return;

		let settleClipboardSend;
		const clipboardSendTracker = new Promise((resolve) => { settleClipboardSend = resolve; });
		clipboardSendInFlight = clipboardSendTracker;
		try {
			// Shared reader (lib/clipboard-sync.js): text/image-normalized, with the
			// DataError->readText() fallback for large text living in one place.
			const res = await readLocalClipboard(enable_binary_clipboard);
			if (res) {
				if (res.kind === 'image') {
					const arrayBuffer = await res.blob.arrayBuffer();
					await sendClipboardData(arrayBuffer, res.mime);
					console.log(`Sent binary clipboard on focus via sendClipboardData: ${res.mime}, size: ${res.blob.size} bytes`);
				} else if (res.text !== lastClipboardText) {
					await sendClipboardData(res.text);
					lastClipboardText = res.text;
					console.log("Sent clipboard text on focus via sendClipboardData");
				}
			}
		} catch (err) {
			if (err.name !== 'NotFoundError' && err.name !== 'DataError' && err.name !== 'NotAllowedError'
				&& !(err.message && err.message.includes('not focused'))) {
				console.warn(`Clipboard read error: ${err.name}`);
			}
		} finally {
			settleClipboardSend();
			if (clipboardSendInFlight === clipboardSendTracker) clipboardSendInFlight = null;
		}
	}

	// Paste-ordering hold + non-Chromium copy/paste gestures live in the shared
	// factory (see lib/clipboard-sync.js); only the gates and the transport's
	// send function are per-core. Wired/unwired with the session lifecycle.
	const clipboardGestures = createClipboardGestures({
		isChromium,
		clipboardSync,
		sendClipboardData: (data, mime) => sendClipboardData(data, mime),
		canSync: () => !isSharedMode && clipboardStatus === "enabled",
		canRead: () => !!clipboard_in_enabled,
		canWrite: () => !!clipboard_out_enabled,
		binaryEnabled: () => !!enable_binary_clipboard,
		getSendInFlight: () => clipboardSendInFlight,
		getDeferredWriteInFlight: () => deferredClipboardWriter.getInFlight(),
	});

	async function handleWindowFocus() {
		webrtc.sendDataChannelMessage("kr");
		// Chromium reads the clipboard on focus without friction. Firefox/WebKit raise an
		// intrusive paste prompt on every focus read, so there the read is driven only by
		// the Ctrl/Cmd+V keydown and paste-event handlers.
		if (isChromium) {
			readLocalClipboardAndSend();
		}
	}


	function handleWindowBlur() {
		// reset keyboard to avoid stuck keys.
		webrtc.sendDataChannelMessage("kr");
	}

	function setupKeyBoardAssisstant() {
		if (isSharedMode) {
			console.log("Shared mode detected, skipping keyboard assistant setup.");
			return;
		}
		const keyboardInputAssist = document.getElementById('keyboard-input-assist');
		if (keyboardInputAssist && input) {
		// Typed characters are handled by the Input class's own 'input' listener
		// on this element (_handleMobileInput); only the control keys mobile
		// keyboards emit as keydown need forwarding here.
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

	async function sendClipboardData(data, mimeType = 'text/plain') {
		if (clipboardStatus !== "enabled" || !clipboard_in_enabled || data == null) return;
		// Change-only sync: skip content the session already carries in either direction.
		if (!clipboardSync.shouldSend(data, mimeType)) return;

		const isBinary = data instanceof ArrayBuffer || data instanceof Uint8Array;
		let dataBytes;
		if (isBinary) {
			dataBytes = data instanceof Uint8Array ? data : new Uint8Array(data);
		} else {
			dataBytes = new TextEncoder().encode(data);
			mimeType = 'text/plain';
		}
		// Shared chunked send (see lib/clipboard-worker-bridge.js) — identical wire
		// protocol and per-chunk worker offload as WebSockets. Transport specifics:
		// the data-channel send + a drain gate (a multi-MB burst overflows the SCTP
		// send buffer and Chromium closes the channel -> whole session dies). Raw
		// chunk sized so its base64 fits the data-channel message budget.
		try {
			await sendClipboardChunked(dataBytes, mimeType, {
				worker: clipboardWorker,
				send: (m) => webrtc.sendDataChannelMessage(m),
				waitDrain: async () => {
					if (webrtc.waitForDataChannelDrain) await webrtc.waitForDataChannelDrain(1024 * 1024);
					return true;
				},
				chunkRawBytes: Math.max(1, Math.floor(dcMessageBudget() * 3 / 4)),
				nextTid: () => ++__clipboardTransferCounter,
			});
		} catch (err) {
			console.error("Error sending clipboard data:", err);
		}
	}

	// Most browsers have limitations on the types of images
	// for clipboard so convert them to widely supported png
	async function convertImageToPngBlob(blob) {
		return new Promise((resolve, reject) => {
			const img = new Image();
			const url = URL.createObjectURL(blob);
			img.onload = () => {
				URL.revokeObjectURL(url);
				const canvas = document.createElement('canvas');
				canvas.width = img.width;
				canvas.height = img.height;
				const ctx = canvas.getContext('2d');
				ctx.drawImage(img, 0, 0);
				canvas.toBlob((pngBlob) => {
					resolve(pngBlob);
				}, 'image/png');
			};
			img.onerror = (err) => {
				URL.revokeObjectURL(url);
				reject(new Error("Failed to load image for PNG conversion"));
			};
			img.src = url;
		});
	}

	const cleanupMultipartClipboard = () => {
		multipartClipboard.mimeType = null;
		multipartClipboard.chunks = [];
		multipartClipboard.totalSize = 0;
		multipartClipboard.inProgress = false;
	};

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
						blob = await convertImageToPngBlob(blob);
						if (!blob) return { isMultipart, mimeType, content: null };
						mimeType = 'image/png';
					}
				} catch (err) {
					console.error("Image conversion failed for clipboard message:", err);
					return { isMultipart, mimeType, content: null };
				}
				return { isMultipart, mimeType, content: new ClipboardItem({ [mimeType]: blob }) };
			case "clipboard-msg-start":
				multipartClipboard.chunks = [];
				multipartClipboard.mimeType = mimeType;
				multipartClipboard.totalSize = msg.data.total_size;
				multipartClipboard.inProgress = true;
				console.log(`Starting multi-part download: ${mimeType}, expected raw size: ${msg.data.total_size}`);
				return { isMultipart: true, mimeType, content: null };
			case "clipboard-msg-data":
				if (multipartClipboard.inProgress) {
					multipartClipboard.chunks.push(msg.data.content);
				}
				return { isMultipart: true, mimeType, content: null };
			case "clipboard-msg-end":
				if (!multipartClipboard.inProgress) {
					return { isMultipart: false, mimeType, content: null };
				}
				const fullBase64 = multipartClipboard.chunks.join("");
				mimeType = multipartClipboard.mimeType;
				try {
					const { result, byteLength } = await clipboardWorker.decode(fullBase64, mimeType);
					if (byteLength !== multipartClipboard.totalSize) {
						console.warn(`Size mismatch! Expected ${multipartClipboard.totalSize}, got ${byteLength}`);
						cleanupMultipartClipboard();
						return { isMultipart: false, mimeType, content: null };
					}
					if (mimeType === 'text/plain') {
						content = result;
					} else {
						let blob = new Blob([result], { type: mimeType });
						if (mimeType.startsWith('image/') && mimeType !== 'image/png') {
							blob = await convertImageToPngBlob(blob);
							if (!blob) {
								cleanupMultipartClipboard();
								return { isMultipart: false, mimeType, content: null };
							}
							mimeType = 'image/png';
						}
						content = new ClipboardItem({ [mimeType]: blob });
					}
				} catch (err) {
					console.error("Worker decoding failed:", err);
				}
				cleanupMultipartClipboard();
				return { isMultipart: false, mimeType, content };
			default:
				console.warn("Unknown clipboard cmd received");
		}
	}

	// Returns URL pathname against browser's URL even when running under
	// iframe context where the pathname could be root directory `/` otherwise.
	function getRoutePrefix() {
		const pathname = window.location.pathname;
		const dirPath = pathname.substring(0, pathname.lastIndexOf('/') + 1);
		return dirPath.replace(/\/$/, '');
	}

	return {
		initialize() {
			InitUI();
			// Create the nodes and configure its attributes
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

			// Editable (not readOnly): the overlay hosts IME composition — browsers
			// never activate an IME on a read-only input. Mirrors the websockets core.
			let overlayInput = document.createElement('input');
			overlayInput.type = 'search';
			overlayInput.readOnly = false;
			overlayInput.autocomplete = 'off';
			overlayInput.id = 'overlayInput';

			// prepare the video and audio elements
			videoElement = document.createElement('video');
			videoElement.id = 'stream';
			videoElement.className = 'video';
			videoElement.autoplay = true;
			videoElement.playsInline = true;
			videoElement.addEventListener('resize', () => {
				// The track's intrinsic size IS the realized server resolution.
				// When it disagrees with what was requested, window-math input
				// mapping (CSS × dpr == server px) is wrong; the flag routes
				// input.js through the video's fitted content box instead.
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
			// Fetch locally stored application data. Reads with fallbacks only and
			// persists nothing: a fresh profile keeps every key unset so server-pushed
			// defaults stay re-pushable. Only genuine user actions (and
			// sanitizeAndStoreSettings for keys the user already overrode) write localStorage.
			appName = "webrtc"
			debug = getBoolParam('debug', false);
			turnSwitch = getBoolParam('turn_switch', false);
			resizeRemote = getBoolParam('resize_remote', resizeRemote);
			scaleLocal = getBoolParam('scaleLocallyManual', !resizeRemote);
			videoBitRate = getFloatParam('video_bitrate', videoBitRate);
			videoFramerate = getIntParam('framerate', videoFramerate);
			audioBitRate = getIntParam('audio_bitrate', audioBitRate);
			window.isManualResolutionMode = getBoolParam('is_manual_resolution_mode', false);
			isGamepadEnabled = getBoolParam('isGamepadEnabled', true);
			manualWidth = getIntParam('manual_width', null);
			manualHeight = getIntParam('manual_height', null);
			encoder = getStringParam('encoder_rtc', 'h264enc');
			rateControlMode = getStringParam('rate_control_mode', 'cbr');
			// hiDPI contract: CSS scaling on => DPR 1 everywhere (resolution + input);
			// off => devicePixelRatio is applied by the resolution senders and input math.
			// Default OFF (ws-core parity, = HIDPI_SPEC fallback) so an auto-resolution HiDPI
			// client renders a crisp physical-res buffer. scaling_dpi is an INDEPENDENT user
			// setting (the DPI slider) — the HiDPI toggle does not touch it.
			useCssScaling = getBoolParam('useCssScaling', false);
			// scaling_dpi default: sync to the local display scaling so remote fonts match local;
			// an explicit slider value wins. Independent of resolution (no manual/auto coupling).
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
				// listen for dashboard messages (Dashboard -> core client)
				window.addEventListener("message", handleMessage);
				// listen for file upload event
				window.addEventListener('requestFileUpload', handleRequestFileUpload);
				// handlers to handle the drop in files/directories for upload
				overlayInput.addEventListener('dragover', handleDragOver);
				overlayInput.addEventListener('drop', handleDrop);
				// re-acquire the screen wake lock when the tab returns to the foreground
				document.addEventListener('visibilitychange', handleVisibilityChange);
			}

			// Additional displays: signaling scopes controller/slot uniqueness per
			// display_id and the server runs one media pipeline per display, so a
			// #display2-<position> page streams its own region of the extended
			// desktop (websockets parity). The position rides the connect metadata.
			const displayId = hash.startsWith('#display2') ? 'display2' : 'primary';
			let displayPosition = 'right';
			if (displayId === 'display2') {
				const posMatch = hash.match(/^#display2-(right|left|up|down)/);
				if (posMatch) displayPosition = posMatch[1];
			}

			// WebRTC entrypoint, connect to the signaling server
			var pathname = getRoutePrefix() + "/";
			var protocol = (location.protocol == "http:" ? "ws://" : "wss://");
			var url = new URL(protocol + window.location.host + pathname + "api/" + appName + "/signaling/");
			// Secure-mode token from the page URL (?token=...); the server matches it
			// against the active mk token to grant a viewer read-write collaboration.
			var authToken = new URLSearchParams(window.location.search).get('token') || undefined;
			// Set on a fatal server verdict (4000/4001): blocks the pc-failure
			// recovery reload so a superseded page can't re-enter the takeover loop.
			let fatalConnectionHalt = false;
			let pcRecoveryTimer = null;
			var signaling = new WebRTCSignaling(url, clientRole, clientSlot, isStrictViewer, authToken, displayId, displayPosition);
			// A plain GET on the signaling endpoint returns 409 exactly when the
			// server is serving WebSockets. After repeated connect failures, probe
			// once and converge the stored mode instead of reload-looping.
			signaling.onfatalretry = async () => {
				let flipGuard = null;
				try { flipGuard = sessionStorage.getItem('selkies_mode_flip'); } catch (e) { /* ignore */ }
				if (!flipGuard) {
					try {
						const probeURL = new URL(url.href);
						probeURL.protocol = (location.protocol === 'http:' ? 'http:' : 'https:');
						const res = await fetch(probeURL.href, { cache: 'no-store' });
						if (res.status === 409) {
							try { sessionStorage.setItem('selkies_mode_flip', '1'); } catch (e) { /* ignore */ }
							setStringParam('stream_mode', 'websockets');
							console.warn('[signaling] Server is serving WebSockets (endpoint 409); switching stored mode.');
						}
					} catch (e) { /* unreachable server: plain reload keeps retrying */ }
				}
				location.reload();
			};
			webrtc = new WebRTCClient(signaling, videoElement, 1, isSharedMode);
			const send = (data) => {
				if (isSharedMode && isStrictViewer) return;
				webrtc.sendDataChannelMessage(data);
			}
			input = new Input(overlayInput, send, isSharedMode, playerInputTargetIndex, useCssScaling);
			// CSS-pixel window size (websockets-core parity): the library default
			// multiplies by devicePixelRatio, and every caller here applies dpr
			// itself — without this override HiDPI sessions double-multiply (4x
			// the pixels at dpr 2) in both the requested resolution and the
			// element sizing.
			input.getWindowResolution = () => {
				const container = videoElement && videoElement.parentElement;
				if (!container) return [window.innerWidth, window.innerHeight];
				const rect = container.getBoundingClientRect();
				return [rect.width, rect.height];
			};
			// Same global handle the websockets core exposes.
			window.webrtcInput = input;

			// Apply persisted input preferences and announce state to the
			// dashboard (parity with the websockets core).
			if (trackpadMode) input.setTrackpadMode(true);
			// Resolves the user preference plus the multi-monitor override (a
			// #display2 page always renders its own cursor).
			applyEffectiveCursorSetting();
			window.postMessage({ type: 'trackpadModeUpdate', enabled: trackpadMode }, window.location.origin);
			window.postMessage({ type: 'clientRoleUpdate', role: clientRole }, window.location.origin);

			setupKeyBoardAssisstant();

			// assign the handlers to respective objects; entries land in the capped
			// window.selkiesLogs buffers and mirror to the console
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
				if (reconnect) {
					status = 'connecting';
					webrtc.reset();
				} else {
					status = 'disconnected';
				}
				updateStatusDisplay();
			};

			signaling.onshowalert = (msg) => {
				// Fatal server verdict (invalid slot / superseded takeover): stay down.
				// The peer connection will go 'failed' shortly after — the recovery
				// timer below must not reload us back into an eviction ping-pong.
				fatalConnectionHalt = true;
				// Suppress the disconnect alert when it's the result of an intentional
				// mode switch: the dashboard sets this flag before requesting /api/switch,
				// which closes the WebRTC peer (code 4000) before the page reloads.
				if (typeof window !== 'undefined' && window.__selkiesModeSwitching) return;
				alert("Disconnected: " + msg + " Please try again.");
			}

			// Send webrtc status and error messages to logs.
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
				// Gpu stats for the Dashboard to render
				window.gpu_stats = stats;
			}

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
					// Re-assert the chosen output device on the (re)connected stream.
					applyOutputDevice();
				} else if (state === "failed" || state === "disconnected") {
					// ICE consent expiry / network loss: once the server tears the
					// pipeline down the screen stays black forever without a fresh
					// SDP exchange — reload to reconnect. 'disconnected' can self-heal
					// on transient loss, so it gets a grace window; 'failed' is final.
					// Never fight a fatal server verdict (superseded/invalid slot) or
					// an intentional mode switch.
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

			webrtc.ondatachannelopen = () => {
				console.log("Data channel opened");
				if (!isStrictViewer) {
					input.ongamepadconnected = (gamepad_id) => {
					let connected = toggleGamepadConnection();
					if (connected) {
						gamepad.gamepadState = "connected";
						gamepad.gamepadName = gamepad_id;
						webrtc._setStatus('Gamepad connected: ' + gamepad_id);
					}
					}
					input.ongamepaddisconnected = () => {
						if (input.gamepadManager !== null) {
							input.gamepadManager.disable();
							gamepad.gamepadState = "disconnected";
							gamepad.gamepadName = "none";
							webrtc._setStatus('Gamepad disconnected');
						}
					}
				}

				// Bind input handlers. For shared mode, the listeners are limited
				input.attach();

				// Pull the current server clipboard once on connect, mirroring the
				// websockets core. Without this a WebRTC session (or one reached by
				// switching transports, which reloads the page) shows no server
				// clipboard — images included — until the next server-side change.
				// The server silently drops a viewer's 'cr', so this is safe in
				// shared mode too.
				try {
					webrtc.sendDataChannelMessage('cr');
				} catch (e) {
					console.warn('Failed to send initial clipboard request (cr):', e);
				}

				if (isSharedMode) {
					console.log('Shared mode: skipping loading of last session settings and sending persisted settings to server');
					return;
				}

				loadLastSessionSettings();
				sendClientPersistedSettings();

				// Send client-side metrics over data channel every 5 seconds
				if (metricsLoopId !== null) clearInterval(metricsLoopId); // avoid duplicate on data channel reopen
				metricsLoopId = setInterval(async () => {
					if (connectionStat.connectionFrameRate === parseInt(connectionStat.connectionFrameRate, 10)) {
						webrtc.sendDataChannelMessage(`_f,${connectionStat.connectionFrameRate}`);
					}
					if (connectionStat.connectionLatency === parseInt(connectionStat.connectionLatency, 10)) {
						webrtc.sendDataChannelMessage(`_l,${connectionStat.connectionLatency}`);
					}
				}, 5000)
			}

			webrtc.ondatachannelclose = () => {
				input.detach();
			}

			// Unified dashboard hotkeys (parity with the websockets core): the core
			// owns the chords; dashboards react to these messages. The legacy
			// built-in drawer still toggles for bare-core sessions.
			input.onmenuhotkey = () => {
				showDrawer = !showDrawer;
				window.postMessage({ type: 'toggleDashboard' }, window.location.origin);
			}
			input.ongamepadhotkey = () => {
				window.postMessage({ type: 'toggleTouchGamepad' }, window.location.origin);
			}

			webrtc.onplaystreamrequired = () => {
				showStart = true;
			}

			if (!isSharedMode) {
				// Actions to take whenever window changes focus
				window.addEventListener('focus', handleWindowFocus);
				window.addEventListener('blur', handleWindowBlur);
				clipboardGestures.wire();
			}

			webrtc.onclipboardcontent = async (msg) => {
				if (!window.isSecureContext || isSharedMode) {
					return;
				}
				if (clipboardStatus === 'enabled') {
					const {isMultipart, mimeType, content} = await handleClipboardData(msg);
					const isText = mimeType === "text/plain";
					if (isMultipart || content === null) {
						return;
					}

					if (isText) {
						clipboardSync.resolveServer(content, null, 'text/plain');
						// Parity with the websockets core: the dashboard UI gets the
						// (bounded) preview regardless; the local clipboard write is
						// gated per-direction (server->client = out) and retried on
						// the next gesture when the browser demands activation.
						window.postMessage(clipboardPreviewMessage(content),
							window.location.origin);
						if (clipboard_out_enabled) {
							deferredClipboardWriter.write(
								() => navigator.clipboard.writeText(content), {
									onSuccess: () => console.log('Successfully wrote text from server to local clipboard.'),
									onFailure: (err) => console.log('Could not copy text to clipboard: ', err),
								});
						}
					} else {
						if (enable_binary_clipboard && clipboard_out_enabled) {
							try { content.getType(mimeType).then(async (b) => clipboardSync.resolveServer(undefined, b, mimeType, new Uint8Array(await b.arrayBuffer()))).catch(() => {}); } catch (_) {}
							deferredClipboardWriter.write(
								() => navigator.clipboard.write([content]), {
									onSuccess: () => {
										window.postMessage({
											type: 'clipboardContentUpdate',
											text: "received an image from server",
										}, window.location.origin);
										console.log(`Successfully wrote image (${mimeType}) from server to local clipboard.`);
										clipboardSync.captureLocalImageSig();
									},
									onFailure: (err) => console.error('Failed to write image to clipboard: ', err),
								});
						}
					}
				}
			}

			webrtc.oncursorchange = (cursorData) => {
				input.updateServerCursor(cursorData);
			}

			webrtc.ondisplayconfig = (config) => {
				// A secondary joining/leaving flips the multi-monitor cursor
				// override on the primary page (websockets parity).
				const displays = (config && config.displays) || [];
				const secondaryConnected = displays.some((d) => d !== 'primary');
				if (isSecondaryDisplayConnected !== secondaryConnected) {
					console.log(`Secondary display connection status changed to: ${secondaryConnected}`);
					isSecondaryDisplayConnected = secondaryConnected;
					applyEffectiveCursorSetting();
				}
			}

			webrtc.onsystemaction = (action) => {
				webrtc._setStatus("Executing system action: " + action);
				if (action === 'reload') {
					setTimeout(() => {
						// trigger webrtc.reset() by disconnecting from the signaling server.
						signaling.disconnect();
					}, 700);
				} else {
					webrtc._setStatus('Server sent acknowledgement for ' + action);
				}
			}

			webrtc.onlatencymeasurement = (latency_ms) => {
				serverLatency = latency_ms * 2.0;
			}

			webrtc.onsystemstats = (stats) => {
				// Dashboard takes care of data validation
				window.system_stats = stats;
			}

			webrtc.onserversettings = (obj) => {
				if (obj.settings === undefined || obj.settings === null) {
					console.warn("Received invalid server settings paylod");
					return;
				}
				console.log("Received server settings payload:", obj.settings);
				const changes = sanitizeAndStoreSettings(obj.settings);
				// Gate 'cmd,' on the server-advertised value, not window.command_enabled
				// (a persisted client pref); absent/malformed => true for older servers.
				const ce = obj.settings && obj.settings.command_enabled;
				serverCommandEnabled = (ce && typeof ce.value === 'boolean') ? ce.value : true;
				// Per-direction clipboard gates are policy, so the server value wins
				// (module mirrors, not window[...], gate the actual handlers).
				const cin = obj.settings && obj.settings.clipboard_in_enabled;
				if (cin && typeof cin.value === 'boolean') clipboard_in_enabled = cin.value;
				const cout = obj.settings && obj.settings.clipboard_out_enabled;
				if (cout && typeof cout.value === 'boolean') clipboard_out_enabled = cout.value;
				// Parity with the websockets core: without this mirror a fresh WebRTC
				// client keeps its default (false) and silently discards server images
				// AND never sends local ones, even with binary clipboard on server-side.
				const ebc = obj.settings && obj.settings.enable_binary_clipboard;
				// User-toggleable: force the gate only when the server locks it;
				// otherwise the stored choice governs (the dashboard toggle and
				// the server-side apply both already follow the stored value).
				if (ebc && typeof ebc.value === 'boolean') {
					enable_binary_clipboard = ebc.locked ? ebc.value : getBoolParam('enable_binary_clipboard', ebc.value);
				}
				window.postMessage({ type: 'serverSettings', payload: obj.settings }, window.location.origin);
				if (Object.keys(changes).length > 0) {
					console.log('Client settings were sanitized by server rules. Sending updates back to server:', changes);
					handleSettingsMessage(changes);
				}
				if (obj.settings.is_manual_resolution_mode && obj.settings.is_manual_resolution_mode.value === true) {
					console.log("Server settings payload confirms manual mode. Switching to manual resize handlers.");
					const serverWidth = obj.settings.manual_width ? parseInt(obj.settings.manual_width.value, 10) : 0;
					const serverHeight = obj.settings.manual_height ? parseInt(obj.settings.manual_height.value, 10) : 0;
					if (serverWidth > 0 && serverHeight > 0) {
						console.log(`Applying server-enforced manual resolution: ${serverWidth}x${serverHeight}`);
						window.isManualResolutionMode = true;
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

			// Enable clipboard sync on capability + secure context for every engine,
			// matching the websockets core. The clipboard-read permission query
			// rejects with TypeError on Firefox/WebKit and reports 'prompt' on
			// Chromium until the user grants persistent access; gating the whole
			// sync (send AND receive) on state === 'granted' silently disabled the
			// clipboard on Chromium over WebRTC while websockets worked. Per-call
			// NotAllowed/NotFound errors are handled at each read with paste
			// fallbacks, and the Chromium focus read still raises its one-time
			// prompt exactly as before.
			if (window.isSecureContext && navigator.clipboard) {
				clipboardStatus = 'enabled';
			}

			// Apply the fetched (or fallback) RTC config and open the connection.
			// A shared function, so a failed TURN fetch still connects: the data channel is
			// what delivers serverSettings, and without it the dashboard never
			// renders its controls or the WebSocket/WebRTC toggle — i.e. it freezes.
			const applyRtcConfigAndConnect = (config) => {
				// for debugging, force use of relay server.
				webrtc.forceTurn = turnSwitch;

				// get initial local resolution
				windowResolution = input.getWindowResolution();
				signaling.currRes = windowResolution;

				if (scaleLocal === false) {
						webrtc.element.style.width = windowResolution[0]/window.devicePixelRatio+'px';
						webrtc.element.style.height = windowResolution[1]/window.devicePixelRatio+'px';
				}

				if (config.iceServers && config.iceServers.length > 1) {
						pushCapped(debugEntries, applyTimestamp("using TURN servers: " + config.iceServers[1].urls.join(", ")));
				} else {
						pushCapped(debugEntries, applyTimestamp("no TURN servers found."));
				}
				webrtc.rtcPeerConfig = config;
				webrtc.connect();
			};

			// Fetch RTC configuration containing STUN/TURN servers.
			fetch(getRoutePrefix() + "/api/turn")
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
					// A 404 here is expected when no TURN server is configured, and is
					// NOT fatal. Fall back to an empty ICE config (host/STUN candidates,
					// which serve LAN/localhost) and still connect, so the data channel —
					// and therefore serverSettings and the mode toggle — comes up rather
					// than leaving the dashboard frozen with no way back to WebSockets.
					pushCapped(debugEntries, applyTimestamp(`TURN config unavailable (${error}); connecting without TURN.`));
					console.warn(`Failed to fetch TURN server details (${error}); continuing without TURN.`);
					applyRtcConfigAndConnect({ iceServers: [] });
				})
		},
		cleanup() {
			// reset the data
			window.isManualResolutionMode = false;
			window.fps = 0;

			// remove the listeners
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

			// temporary workaround to nullify/reset the variables
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
			// clear polling timers so they don't leak/fire on null webrtc after reconnect
			if (statsLoopId !== null) { clearInterval(statsLoopId); statsLoopId = null; }
			if (metricsLoopId !== null) { clearInterval(metricsLoopId); metricsLoopId = null; }
			webrtc = null;
			input = null;
			useCssScaling = false;
			detectedSharedModeType = null;
			playerInputTargetIndex = 0;
			enableWebrtcStatics = false;
			enable_binary_clipboard = true;
			// Reset the command gate to its default-true semantics for the next session.
			serverCommandEnabled = true;
			multipartClipboard = {
				chunks: [],
				mimeType: '',
				totalSize: 0,
				inProgress: false
			};

		}
	}
}