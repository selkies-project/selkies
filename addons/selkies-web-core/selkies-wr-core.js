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
import { createClipboardSync, createClipboardGestures, createDeferredClipboardWriter, createLocalClipboardSender, createMultipartClipboardState, createTaggedClipboardFetch, clipboardPreviewMessage, reencodeBlobAsPng } from "./lib/clipboard-sync.js";
import { createFileUploader } from "./lib/file-upload.js";
// Inline (base64 blob) so the worker travels inside selkies-core.js itself —
// no separate hashed file to place next to whichever chunk references it.
import { ClipboardWorkerBridge, sendClipboardChunked } from './lib/clipboard-worker-bridge.js'
import { detectKeyboardLayout } from './lib/keyboard-layout.js';
import { installAuthGuard } from './lib/auth-guard.js';
import { storageKeyForServerKey } from './lib/conditional-settings.js';
import { getRoutePrefix, getStorageAppName } from './lib/util.js';

installAuthGuard();

// Best-effort local keyboard layout, resolved once at script init so the value
// is ready by the time signaling + ICE bring the data channel up (getLayoutMap
// resolves in microtask time next to that). If it somehow loses the race the
// hint simply rides the next SETTINGS send. null = unknown = omit.
let detectedKeyboardLayout = null;
let persistentSettingsSent = false;
// The WebRTCClient lives inside the exported function's scope, so a module-scope
// holder mirrors it for module-scope consumers (the late layout send below).
let activeWebrtcClient = null;
detectKeyboardLayout().then((layout) => {
    detectedKeyboardLayout = layout;
    // This core sends its settings once per session; a probe landing later
    // would leave the hint out for the whole session, so it follows here.
    if (layout && persistentSettingsSent && activeWebrtcClient) {
        try {
            activeWebrtcClient.sendDataChannelMessage(`SETTINGS,${JSON.stringify({ keyboardLayout: layout })}`);
        } catch (e) { /* session may not be up yet */ }
    }
});

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
	// Video bitrate in kbps.
	let videoBitRate = 8000;
	let videoFramerate = 60;
	// Audio bitrate in bps.
	let audioBitRate = 128000;
	let showStart = false;
	let showDrawer = false;
	// Log/debug entries are retained in capped ring buffers (devtools inspection via
	// window.selkiesLogs); everything is also mirrored to the console as it happens.
	// Cap so the log buffers can't grow for the whole session.
	const MAX_LOG_ENTRIES = 1000;
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
	// Set on a fatal server verdict (4000/4001): blocks every recovery reload —
	// the peer-connection one and the resume watchdog's — so a superseded page
	// cannot re-enter the takeover loop.
	let fatalConnectionHalt = false;
	// Last stream resolution asked of the server, in physical stream pixels;
	// compared against the track's intrinsic size to detect a realized size
	// that differs from the request (mode snapping / rejected resize).
	var lastRequestedStreamRes = null;
	// Screen Wake Lock sentinel + preferred audio output device (parity with the WS core).
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
	// Resize debounce delay in milliseconds.
	let rdelta = 500;
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
	// Cursor-rendering preference in force. Seeded from localStorage at connect,
	// then updated by an explicit dashboard pick (which persists) or by a
	// server-pushed value (which does not, so a later server-side change stays
	// re-pushable). The effective value adds the multi-monitor override.
	let useBrowserCursors = true;
	// Whether a secondary display page is connected (server display_config_update
	// broadcast). Multi-monitor forces browser-cursor rendering: the server-drawn
	// cursor overlay only tracks one capture region.
	let isSecondaryDisplayConnected = false;
	// Round resolutions to multiples of 16 (encoder macroblock alignment) instead
	// of the default 2 when the force_aligned_resolution setting is on.
	let force_aligned_resolution = false;

	let enable_binary_clipboard = true;
	// Multipart download state + connect-time cache-only fetch ('cr') tracking:
	// shared factories (lib/clipboard-sync.js), identical in the WebSocket core.
	const multipartClipboard = createMultipartClipboardState();
	let clipboardWorker = new ClipboardWorkerBridge();
	const taggedClipboardFetch = createTaggedClipboardFetch();
	const armTaggedClipboardReply = () => taggedClipboardFetch.arm();
	const consumeInitClipboardFetch = () => taggedClipboardFetch.consume();
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
		// userAgentData brands are the authoritative Chromium signal;
		// window.chrome is only a fallback for older engines, as not every
		// Chromium build defines it.
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
	// Secure-mode collab: an mk-token viewer is granted the full input context
	// via the server's mk_access system action (websockets MK_ACCESS parity).
	let collabInputGranted = false;

	const storageAppName = getStorageAppName();
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
	// Fraction-preserving variant: range bounds stay float-capable.
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
		const userPreference = useBrowserCursors;
		const isDisplay2 = window.location.hash.startsWith('#display2');
		const isMultiMonitorActive = (isDisplay2 || isSecondaryDisplayConnected);
		const finalSetting = isMultiMonitorActive ? true : userPreference;
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
				console.warn(`Could not acquire Wake Lock: ${err.name}, ${err.message}`);
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

	// A backgrounded tab drops the wake lock automatically; re-acquire when
	// visible. A hidden tab also pauses its own video feed server-side
	// (per-peer STOP_VIDEO / START_VIDEO on the data channel, websockets
	// parity): the browser throttles a hidden tab's rendering anyway, so the
	// encode and bandwidth are pure waste. The pause is deferred because a
	// navigating/reloading document reports hidden just before it unloads and
	// timers never fire in an unloading document, so only a genuine tab-hide
	// sends it. Recovery on resume is the server's IDR (plus PLI) — the client
	// sends no keyframe requests of its own. Shared/#player viewer pages register
	// it too: the verbs are viewer-allowed and gate only this peer's RTP sender,
	// so a hidden viewer stops wasting encode/bandwidth on its own feed while the
	// controller and any other viewers keep streaming (the shared capture only
	// stops once every consumer is paused).
	let hiddenVideoPauseTimer = null;
	let videoPausedForHiddenTab = false;
	// The resume is one message on a data channel that can be closed exactly then,
	// and a lost one is answered with nothing: the peer stays subscribed to a feed
	// nobody encodes and the picture never returns. So a tab that came back proves
	// the frames followed — the websockets core's START_VIDEO watchdog, on the
	// signal WebRTC has: the element's playback clock.
	let resumeWatchdogTimer = null;
	let resumeWatchdogAttempts = 0;
	const RESUME_WATCHDOG_MS = 3000;
	const RESUME_WATCHDOG_MAX_ATTEMPTS = 3;

	function clearResumeWatchdog() {
		if (resumeWatchdogTimer !== null) {
			clearTimeout(resumeWatchdogTimer);
			resumeWatchdogTimer = null;
		}
		resumeWatchdogAttempts = 0;
	}

	function armResumeWatchdog() {
		if (resumeWatchdogTimer !== null) clearTimeout(resumeWatchdogTimer);
		const mark = videoElement ? videoElement.currentTime : 0;
		resumeWatchdogTimer = setTimeout(() => checkResumed(mark), RESUME_WATCHDOG_MS);
	}

	function checkResumed(mark) {
		resumeWatchdogTimer = null;
		// Hidden again: the visibility path owns that state.
		if (document.hidden || !webrtc) { resumeWatchdogAttempts = 0; return; }
		if (videoElement && videoElement.currentTime > mark) {
			resumeWatchdogAttempts = 0;
			return;
		}
		// A media element the browser paused while the tab was away plays nothing
		// however much RTP arrives.
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

	async function handleVisibilityChange() {
		if (document.hidden) {
			clearResumeWatchdog();
			if (hiddenVideoPauseTimer === null) {
				hiddenVideoPauseTimer = setTimeout(() => {
					hiddenVideoPauseTimer = null;
					if (!document.hidden || videoPausedForHiddenTab || !webrtc) return;
					// A feed the user already stopped from the dashboard is left
					// alone: pausing it here would make the resume on show
					// resurrect a stream the user turned off.
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
			// Nothing was paused here, but the browser can still have stopped the
			// element itself while the tab was away.
			videoElement.play().catch(() => {});
		}
		if (wakeLockSentinel === null) {
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
			// The user's pick may live under a different name than the server's key
			// (HiDPI stores as useCssScaling), and the override check must read the
			// key the dashboard actually writes, or an unlocked operator value wins
			// forever.
			const storeKey = storageKeyForServerKey(key);
			const finalKey = storageKeyFor(storeKey);
			const wasUnset = window.localStorage.getItem(finalKey) === null;

			if (setting.min !== undefined && setting.max !== undefined) {
				// Float-aware: fractional ranges must not be
				// parsed as ints — that reads "0.5" as 0, flags it out of range,
				// and wipes the pick back to the server default on every connect.
				// In-range stored values are kept verbatim (no write-back).
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
					const clientValue = getBoolParam(storeKey, serverValue);
					window[key] = clientValue;
					setBoolParam(storeKey, clientValue);
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
			'framerate', 'encoder', 'is_manual_resolution_mode',
			'audio_bitrate', 'video_bitrate', 'scaling_dpi', 'enable_binary_clipboard',
			'rate_control_mode', 'video_crf', 'use_cpu', 'force_aligned_resolution',
			'video_fullcolor', 'video_streaming_mode', 'use_paint_over_quality',
			'video_paintover_crf', 'video_paintover_burst_frames'
		];
		const booleanSettingKeys = [
			'is_manual_resolution_mode', 'enable_binary_clipboard', 'use_cpu',
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
		// Seed the DPR-derived scaling_dpi into the very FIRST payload: without
		// it the server brings the desktop up at its default DPI and the
		// dashboard's derived correction ~1s later forces a second (Wayland)
		// capture restart on every HiDPI connect. A user-pinned preset was
		// already collected from localStorage by the loop above and wins;
		// scalingDPI itself is stored-else-DPR-derived at init.
		if (settingsToSend['scaling_dpi'] === undefined) {
			settingsToSend['scaling_dpi'] = scalingDPI;
		}
		if (detectedKeyboardLayout) {
			settingsToSend['keyboardLayout'] = detectedKeyboardLayout;
		}
		settingsToSend['useCssScaling'] = useCssScaling;

		try {
			const settingsJson = JSON.stringify(settingsToSend);
			webrtc.sendDataChannelMessage(`SETTINGS,${settingsJson}`);
			persistentSettingsSent = true;
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
			videoElement.style.objectFit = 'contain';
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
			// 'fill' ignores the aspect ratio; the stream already matches the box.
			videoElement.style.objectFit = 'fill';
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
	// scaling_dpi stops, in 25% steps. Snapping to the NEAREST puts a density the
	// stops do not name (a 3.5x phone, a 133% desktop) on the closest one, and
	// clamps at both ends.
	const DPI_STOPS = [96, 120, 144, 168, 192, 216, 240, 264, 288];

	function autoDeriveDpi() {
		const dpr = window.devicePixelRatio || 1;
		const target = Math.round(dpr * 4) * 24;
		return DPI_STOPS.reduce((prev, cur) =>
			Math.abs(cur - target) < Math.abs(prev - target) ? cur : prev);
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
		// Requested-dimension cap (ws-core parity): a dpr-2 4K fullscreen must
		// not ask the server for a 7680-wide framebuffer.
		if (realWidth > 4080) realWidth = 4080;
		if (realHeight > 4080) realHeight = 4080;
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
			// enable_resize=false pins the PRIMARY's resolution server-side:
			// the resize it ignores must not be requested nor restyled onto.
			// A secondary's resize stays allowed (server gate matches).
			if (window.enable_resize === false && storageDisplayId !== 'display2') {
				return;
			}
			windowResolution = input.getWindowResolution();
			// Clamp the CSS-px size so the physical request stays within the
			// 4080 cap and the element box matches what the server realizes
			// (mirrors ws-core handleResizeUI).
			const dpr = useCssScaling ? 1 : (window.devicePixelRatio || 1);
			if (windowResolution[0] * dpr > 4080) windowResolution[0] = Math.floor(4080 / dpr);
			if (windowResolution[1] * dpr > 4080) windowResolution[1] = Math.floor(4080 / dpr);
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
		// handle_scaling -> set_dpi; the initial SETTINGS payload's scaling_dpi seed goes through
		// the same idempotent path, so whichever lands first wins and the other no-ops). This is
		// the desktop-font sync, unrelated to the resolution.
		if (webrtc) { try { webrtc.sendDataChannelMessage(`s,${scalingDPI}`); } catch (_) {} }
		// Re-assert persisted trackpad mode's cursor compositing (websockets parity:
		// touch has no hover cursor, so the pointer must be baked into the video).
		if (trackpadMode && webrtc) {
			try { webrtc.sendDataChannelMessage('SET_NATIVE_CURSOR_RENDERING,1'); } catch (_) {}
		}
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
			// A pinned primary keeps the server's resolution: the initial
			// element styling above still applies, the request does not.
			if (window.enable_resize !== false || storageDisplayId === 'display2') {
				sendResolutionToServer(currentWindowRes[0], currentWindowRes[1]);
			}
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

	// callback invoked when "message" event is triggered
	function handleMessage(event) {
		if (event.origin !== window.location.origin) {
			console.warn("Received message from unexpected origin");
			return;
		}
		let message = event.data;
		switch(message.type) {
			case "setScaleLocally":
				// Shared-mode parity with ws-core: a viewer must not drive resolution policy.
				if (isSharedMode) { break; }
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
				if (isSharedMode) { break; }
				console.log("Resetting to window size");
				// Clear the manual dimensions before re-deriving from the window.
				manualHeight = manualWidth = 0;
				// With the primary pinned (enable_resize=false) the stream is
				// not moving: only return to auto mode, without a resend or
				// restyle (resizeEnd gates later resizes itself).
				if (window.enable_resize !== false || storageDisplayId === 'display2') {
					let currentWindowRes = input.getWindowResolution();
					resetToWindowResolution(...currentWindowRes);
					sendResolutionToServer(...currentWindowRes);
				}
				enableAutoResize();
				// snake_case keys: these are the ones read back at init.
				setIntParam('manual_width', null);
				setIntParam('manual_height', null);
				setBoolParam('is_manual_resolution_mode', false);
				window.isManualResolutionMode = false;
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
				if (isSharedMode) { break; }
				if (typeof message.value === 'boolean') {
					const changed = useCssScaling !== message.value;
					useCssScaling = message.value;
					// persist === false marks a server-authored value: apply it, but
					// leave the user's own key alone so their pick survives the lock.
					if (message.persist !== false) {
						setBoolParam('useCssScaling', useCssScaling);
					}
					console.log(`Set useCssScaling to ${useCssScaling}${message.persist === false ? '.' : ' and persisted.'}`);
					if (input && typeof input.updateCssScaling === 'function') {
						input.updateCssScaling(useCssScaling);
					}
					if (changed) {
						updateVideoImageRendering();
						if (window.isManualResolutionMode && manualWidth != null && manualHeight != null) {
							sendResolutionToServer(manualWidth, manualHeight);
							applyManualStyle(manualWidth, manualHeight, scaleLocal);
						} else if (!isSharedMode && input &&
							(window.enable_resize !== false || storageDisplayId === 'display2')) {
							// enable_resize=false pins the PRIMARY to the server's
							// resolution: the resize it ignores must not be
							// requested, and the layout must not restyle onto a
							// size whose source never moves. Secondaries stay
							// allowed (ws-core and server parity).
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
				// Defense in depth (ws-core parity): a shared/strict-viewer page never
				// reaches the server-side 'cmd,' execution path.
				if (isSharedMode) { break; }
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
					// Same per-peer server gate the tab-hide pause uses: the sender
					// stops RTP for THIS peer, capture stops when every consumer is
					// paused, and resume comes back with an IDR.
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
					// Audio stays negotiated for the session; the toggle is a local
					// element mute (the bundled <video> carries the audio track).
					const audioOn = !!message.enabled;
					videoElement.muted = !audioOn;
					isAudioPipelineActive = audioOn;
					window.postMessage({ type: 'pipelineStatusUpdate', audio: audioOn }, window.location.origin);
					postSidebarButtonUpdate();
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
			case 'clipboardImageUpdate': {
				// Dashboard image upload: hand the blob to the same binary path the
				// focus/paste read uses. Only meaningful when binary clipboard is on
				// (the server drops image writes otherwise). Every skip surfaces a
				// notification — a dead click with no feedback reads as a bug.
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
				(async () => {
					try {
						const buf = await message.imageBlob.arrayBuffer();
						await sendClipboardData(buf, message.imageBlob.type || 'image/png', notifyClipboardImageSkip);
					} catch (e) {
						console.warn('Failed to send uploaded clipboard image:', e);
						notifyClipboardImageSkip('send failed: ' + e.message, 'clipboardSkipSendFailed');
					}
				})();
				break;
			}
			case 'audioDeviceSelected':
				if (message.context === 'output' && message.deviceId) {
					preferredOutputDeviceId = message.deviceId;
					applyOutputDevice();
				} else if (message.context === 'input' && message.deviceId) {
					preferredInputDeviceId = message.deviceId;
					// A live mic must move to the new device: cycle the track.
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
					useBrowserCursors = message.value;
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
					// Touch has no hover cursor: composite the pointer into the
					// video (websockets parity).
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

	function handleSettingsMessage(settings, fromServer) {
		// A server-authored payload (the locked/overridden values replayed on every
		// connect) is applied to the runtime but never written to the user's own
		// keys, where it would outlive the lock and masquerade as their pick. Only a
		// dashboard-authored payload persists.
		const storeInt = fromServer ? () => {} : setIntParam;
		const storeBool = fromServer ? () => {} : setBoolParam;
		const storeString = fromServer ? () => {} : setStringParam;
		// Debug toggle parity with the websockets core: persist and reload so the
		// verbose log buffers/flips apply at every layer (server push included).
		if (settings.debug !== undefined) {
			debug = settings.debug;
			// Persisted even for a server-authored value: the reload below only
			// settles once the flag is already in storage.
			setBoolParam('debug', debug);
			console.log(`Applied debug setting: ${debug}. Reloading...`);
			setTimeout(() => { window.location.reload(); }, 700);
			return;
		}
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
		if (settings.encoder !== undefined) passthrough.encoder = settings.encoder;
	// A secondary page's runtime move to another side of the primary (WS
	// displayPosition parity): the server applies and re-lays out; unguarded
	// for the primary, which ignores it server-side.
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
			// The server restarts the pipeline with the new encoder (forwarded via the
			// SETTINGS passthrough above); track it locally for the decode path.
			encoder = settings.encoder;
			storeString('encoder', encoder);
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
			// Route a server-locked/overridden HiDPI value through the same flow
			// as the dashboard toggle (websockets core parity), so the operator's
			// setting reaches every layer that reacts to the toggle.
			handleMessage({
				origin: window.location.origin,
				data: { type: 'setUseCssScaling', value: !!settings.use_css_scaling, persist: !fromServer },
			});
		}
		if (settings.use_browser_cursors !== undefined) {
			// Server-pushed (locked/overridden) value: applied to the input layer
			// through the same re-derivation as the dashboard toggle, but never
			// written to the user's key, where it would masquerade as their pick
			// after an unlock. Only the setUseBrowserCursors message persists.
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
			// Re-assert the current resolution so the stream snaps to the new
			// alignment without waiting for the next window resize. A pinned
			// primary (enable_resize=false) keeps the server's resolution.
			if (window.isManualResolutionMode && manualWidth != null && manualHeight != null) {
				sendResolutionToServer(manualWidth, manualHeight);
			} else if (!isSharedMode && input &&
				(window.enable_resize !== false || storageDisplayId === 'display2')) {
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
		// Non-racy gate: already running.
		if (statsLoopId !== null) return;
		// Set synchronously, before any async work.
		statWatchEnabled = true;
		let statsTickBusy = false;
		statsLoopId = setInterval(async () => {
			// Skip a tick whose predecessor still awaits getStats() (busy pc, GC
			// pause): overlapping ticks would double-update the byte baselines.
			if (statsTickBusy) return;
			statsTickBusy = true;
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
			} finally {
				statsTickBusy = false;
			}
		// Stats refresh interval (1000 ms)
		}, 1000);
	}

	// Focus/gesture local->server sync (lib/clipboard-sync.js), identical in the
	// WebSocket core; text re-sends are deduped here client-side.
	const localClipboardSender = createLocalClipboardSender({
		isChromium,
		getDeferredWriteInFlight: () => deferredClipboardWriter.getInFlight(),
		isSharedMode: () => isSharedMode,
		canSync: () => clipboardStatus === "enabled",
		canRead: () => !!clipboard_in_enabled,
		binaryEnabled: () => !!enable_binary_clipboard,
		sendClipboardData: (data, mime) => sendClipboardData(data, mime),
		dedupeText: true,
	});
	const readLocalClipboardAndSend = () => localClipboardSender.readAndSend();
	const maybeSendInitialClipboard = () => localClipboardSender.maybeInitial();

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
		getSendInFlight: () => localClipboardSender.getSendInFlight(),
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

	// A skipped clipboard-image upload tells the dashboard why, in its
	// notification channel (the same {type:'fileUpload',status:'warning'} shape
	// transfer warnings already use).
	function notifyClipboardImageSkip(reason, code) {
		console.warn('Clipboard image upload skipped: ' + reason);
		window.postMessage({
			type: 'fileUpload',
			payload: { status: 'warning', fileName: 'clipboard-image', message: reason, code },
		}, window.location.origin);
	}

	async function sendClipboardData(data, mimeType = 'text/plain', onSkip = null) {
		const skip = (reason, code) => { if (onSkip) onSkip(reason, code); };
		if (clipboardStatus !== "enabled" || !clipboard_in_enabled || data == null) {
			skip('clipboard-in disabled', 'clipboardSkipInDisabled');
			return;
		}
		if (!webrtc || !webrtc.dataChannelOpen()) {
			skip('not connected', 'clipboardSkipNotConnected');
			return;
		}
		// Change-only sync: skip content the session already carries in either direction.
		if (!clipboardSync.shouldSend(data, mimeType)) {
			skip('already the current clipboard', 'clipboardSkipUnchanged');
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
			// sendDataChannelMessage drops quietly on a closed channel, so a
			// mid-transfer death completes without a throw: keep the content
			// re-sendable and say why nothing arrived.
			if (!webrtc.dataChannelOpen()) {
				skip('connection lost during send', 'clipboardSkipSendFailed');
				return;
			}
			// Only a completed transfer marks the content synced; a throw above
			// leaves it re-sendable on the next copy of the same content.
			clipboardSync.markSynced(data, mimeType);
		} catch (err) {
			console.error("Error sending clipboard data:", err);
			skip('send failed: ' + (err && err.message ? err.message : err),
				'clipboardSkipSendFailed');
		}
	}

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
						// ClipboardItem accepts only image/png on write (shared
						// re-encode; see lib/clipboard-sync.js).
						blob = await reencodeBlobAsPng(blob);
						mimeType = 'image/png';
					}
				} catch (err) {
					console.error("Image conversion failed for clipboard message:", err);
					return { isMultipart, mimeType, content: null };
				}
				// Insecure origins have no ClipboardItem; text still caches above.
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
				const assembled = multipartClipboard.assemble();
				mimeType = assembled.mimeType;
				const fullBase64 = assembled.base64;
				try {
					const { result, byteLength } = await clipboardWorker.decode(fullBase64, mimeType);
					if (byteLength !== assembled.totalSize) {
						console.warn(`Size mismatch! Expected ${assembled.totalSize}, got ${byteLength}`);
						return { isMultipart: false, mimeType, content: null };
					}
					if (mimeType === 'text/plain') {
						content = result;
					} else if (typeof ClipboardItem === 'undefined') {
						// Insecure origins have no ClipboardItem for image payloads.
						content = null;
					} else {
						let blob = new Blob([result], { type: mimeType });
						if (mimeType.startsWith('image/') && mimeType !== 'image/png') {
							blob = await reencodeBlobAsPng(blob);
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
			videoBitRate = getIntParam('video_bitrate', videoBitRate);
			videoFramerate = getIntParam('framerate', videoFramerate);
			audioBitRate = getIntParam('audio_bitrate', audioBitRate);
			window.isManualResolutionMode = getBoolParam('is_manual_resolution_mode', false);
			isGamepadEnabled = getBoolParam('isGamepadEnabled', true);
			manualWidth = getIntParam('manual_width', null);
			manualHeight = getIntParam('manual_height', null);
			encoder = getStringParam('encoder', 'h264enc');
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
			}
			// Per-peer tab-visibility video pause and wake-lock re-acquire apply to
			// every page — controllers and shared/#player viewers alike.
			document.addEventListener('visibilitychange', handleVisibilityChange);

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
			fatalConnectionHalt = false;
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
			// (constructor takes 3 args; strict-viewer gating lives in the send wrapper below)
			webrtc = new WebRTCClient(signaling, videoElement, 1);
			activeWebrtcClient = webrtc;
			const send = (data) => {
				if (isSharedMode && isStrictViewer && !collabInputGranted) return;
				webrtc.sendDataChannelMessage(data);
			}
			input = new Input(overlayInput, send, isSharedMode, playerInputTargetIndex, useCssScaling);
			if (!isSharedMode) {
				// Actions to take whenever window changes focus
				window.addEventListener('focus', handleWindowFocus);
				window.addEventListener('blur', handleWindowBlur);
				// Registered before input attaches (both capture on window,
				// registration order decides), so the paste-ordering hold sees
				// a Ctrl+V before Input forwards it.
				clipboardGestures.wire();
			}
			// Page-lifetime input binding: the overlay must host IME composition
			// before negotiation ends, reconnects reuse it, and sends into a
			// closed channel drop quietly in sendDataChannelMessage.
			// Strict-viewer sends stay gated in the send wrapper.
			input.attach();
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
				// Same probe as the websockets close: an auth wall dropped this
				// session and a fresh document is what re-presents the login.
				if (window.__selkiesAuthProbe) window.__selkiesAuthProbe();
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

				// Input is bound for the page lifetime at construction, so a
				// reopened channel needs no rebind.

				// Pull the current server clipboard once on connect (cache-only),
				// mirroring the websockets core. Without this a WebRTC session (or
				// one reached by switching transports, which reloads the page)
				// shows no server clipboard — images included — until the next
				// server-side change. The server silently drops a viewer's 'cr',
				// so this is safe in shared mode too.
				try {
					taggedClipboardFetch.armLegacyWindow(5000);
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
				// Avoid a duplicate loop when the data channel reopens.
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

			webrtc.onclipboardcontent = async (msg) => {
				// A tagging server marks the payload answering this client's own
				// fetch with reply_to (currently only 'cr') on the single message
				// or the multipart start: cache-only, and the timed heuristic is
				// retired for the rest of the session. Armed before the shared
				// early-return so the session state is consistent either way.
				if (msg.data && msg.data.reply_to === 'cr') armTaggedClipboardReply();
				if (isSharedMode) {
					return;
				}
				// Cache/settle unconditionally (ws-core parity): gating parsing on
				// clipboardStatus made the first payload depend on server_settings
				// ordering on the data channel. Only the LOCAL clipboard write is
				// gated — on enablement, direction policy, and the connect-time
				// 'cr' reply being cache-only.
				// The fetch flag is consumed before the async decode (ws-core
				// parity): arrival order, not decode duration, decides which
				// payload settles the init fetch.
				const isInitClipboardFetch = consumeInitClipboardFetch();
				const {isMultipart, mimeType, content} = await handleClipboardData(msg);
				const isText = mimeType === "text/plain";
				if (isMultipart || content === null) {
					return;
				}
				const canWriteLocal = !isInitClipboardFetch &&
					clipboardStatus === 'enabled' && clipboard_out_enabled;

				if (isText) {
					// Freshness is computed before resolveServer records the sig:
					// a value the session already carries must not churn the
					// local clipboard again.
					const isFreshContent = clipboardSync.shouldSend(content, 'text/plain');
					clipboardSync.resolveServer(content, null, 'text/plain');
					// The dashboard UI gets the (bounded) preview regardless; the
					// local write is retried on the next gesture when the browser
					// demands activation.
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
						const bytes = new Uint8Array(await b.arrayBuffer());
						isFreshImage = clipboardSync.shouldSend(bytes, mimeType);
						clipboardSync.resolveServer(undefined, b, mimeType, bytes);
					} catch (_) {}
					if (canWriteLocal && isFreshImage) {
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
				} else if (action.startsWith('mk_access,')) {
					// Secure-mode collab verdict (websockets MK_ACCESS parity):
					// grant attaches the full input context; revocation detaches
					// it and re-closes the strict-viewer send gate.
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
					// A client-requested command failed server-side (apps modal
					// installs above all): surface it in the same warning channel
					// clipboard skips use, or the optimistic UI reads as success.
					window.postMessage({
						type: 'fileUpload',
						payload: {
							status: 'warning',
							fileName: 'command',
							message: action.slice('command_error,'.length),
							code: 'commandFailed',
						},
					}, window.location.origin);
				} else if (action.startsWith('auth_success,') || action.startsWith('role_update,')) {
					// Secure-mode role verdict (websockets AUTH_SUCCESS / ROLE_UPDATE
					// parity). The server decides role and gamepad slot; the hash-derived
					// guess made at load time is only a default, so a token client must
					// adopt the verdict or it drives a controller UI whose input the
					// server drops, and its gamepad reports land on the wrong slot.
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
						// Only a live slot change gates polling (websockets ROLE_UPDATE
						// parity): the initial verdict carries slot null outside secure
						// mode, which means "slots unmanaged", not a revoked controller,
						// and input.controllerSlot already falls back to the player index.
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
					// Realized-size reconciliation (websockets stream_resolution
					// parity): the server may snap or clamp a request (16-px
					// alignment, compositor refusal), so manual-mode bookkeeping
					// must follow what was actually realized — otherwise the UI
					// keeps advertising, and re-requesting, a size the server
					// cannot produce. The <video> itself follows the RTP track's
					// intrinsic size either way.
					const dims = action.slice('resolution,'.length).split('x');
					const rw = parseInt(dims[0], 10);
					const rh = parseInt(dims[1], 10);
					if (rw > 0 && rh > 0 && window.isManualResolutionMode &&
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
				// Deployment policy the resize paths read (window.enable_resize):
				// a pinned false means requesting or restyling toward a new size
				// is pointless. Absent/malformed keeps the historic enabled default.
				const er = obj.settings && obj.settings.enable_resize;
				if (er && typeof er.value === 'boolean') window.enable_resize = er.value;
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
				// Clipboard gates are now in place: push the user's pre-copied
				// local content once so their first paste isn't stale.
				maybeSendInitialClipboard();
				window.postMessage({ type: 'serverSettings', payload: obj.settings }, window.location.origin);
				if (Object.keys(changes).length > 0) {
					console.log('Client settings were sanitized by server rules. Sending updates back to server:', changes);
					handleSettingsMessage(changes, true);
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
						// windowResolution is already CSS px (getWindowResolution
						// override above); dividing by devicePixelRatio again would
						// leave the element 1/dpr too small until the first restyle.
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

			// Reset the session-scoped state so a later connect starts clean.
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
			clearResumeWatchdog();
			webrtc = null;
			activeWebrtcClient = null;
			input = null;
			useCssScaling = false;
			detectedSharedModeType = null;
			playerInputTargetIndex = 0;
			enableWebrtcStatics = false;
			enable_binary_clipboard = true;
			// Reset the command gate to its default-true semantics for the next session.
			serverCommandEnabled = true;
			multipartClipboard.reset();

		}
	}
}