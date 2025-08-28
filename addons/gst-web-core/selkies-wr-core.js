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

import { WebRTCDemo } from "./lib/webrtc";
import { WebRTCDemoSignaling } from "./lib/signaling"
import { stringToBase64 } from "./lib/util";
import { Input } from "./lib/input"

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
	let videoBitRate = 8000;
	let videoFramerate = 60;
	let audioBitRate = 128000;
	let showStart = false;
	let showDrawer = false;
	// TODO: how do we want to handle the log and debug entries
	let logEntries = [];
	let debugEntries = [];
	let status = 'connecting';
	let clipboardStatus = 'disabled';
	let windowResolution = "";
	let encoderLabel = "";
	let encoder = ""
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
	let isGamepadEnabled = false;

	var videoConnected = "";
	var audioConnected = "";
	var statWatchEnabled = false;
	var webrtc = null;
	var input = null;
	let useCssScaling = false;

	const UPLOAD_CHUNK_SIZE = 64 * 1024  - 1; // 64KiB, excluding a byte for prefix

	// Set storage key based on URL
	const urlForKey = window.location.href.split('#')[0];
	const storageAppName = urlForKey.replace(/[^a-zA-Z0-9.-_]/g, '_');

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

	// Function to add timestamp to logs.
	var applyTimestamp = (msg) => {
		var now = new Date();
		var ts = now.getHours() + ":" + now.getMinutes() + ":" + now.getSeconds();
		return "[" + ts + "]" + " " + msg;
	}

	const roundDownToEven = (num) => {
		return Math.floor(num / 2) * 2;
	};

	function playStream() {
		console.log("clicked playbutton")
		showStart = false;
		if (playButtonElement) playButtonElement.classList.add('hidden');
		webrtc.playStream();
	}

	function updateStatusDisplay() {
		if (statusDisplayElement) {
			statusDisplayElement.textContent = status;
			if (status == 'connected') {
				// clear the status and show the play button
				statusDisplayElement.classList.add("hidden");
				if (playButtonElement) {
					if (showStart) playButtonElement.classList.add("hidden")
					else playButtonElement.classList.remove("hidden")
				}
			}
		}
	}

	function updateVideoImageRendering(){
		if (!videoElement) return;

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

	function applyManualStyle(targetWidth, targetHeight, scaleToFit) {
		if (targetWidth <=0 || targetHeight <=0) {
			console.log("Invalid target height or width")
			return;
		}

		const dpr = (window.isManualResolutionMode || useCssScaling) ? 1 : (window.devicePixelRatio || 1);
		const logicalWidth = roundDownToEven(targetWidth * dpr);
		const logicalHeight = roundDownToEven(targetHeight * dpr);
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
			videoElement.style.position = 'absolute';
			videoElement.style.width = `${targetWidth}px`;
			videoElement.style.height = `${targetHeight}px`;
			videoElement.style.top = '0px';
			videoElement.style.left = '0px';
			videoElement.style.objectFit = 'fill'; // Use 'fill' to ignore aspect ratio
			console.log(`Applied manual style (Exact): CSS ${targetWidth}x${targetHeight}, Pos 0,0`);
		}
		updateVideoImageRendering();
	}

	function resetToWindowResolution(targetWidth, targetHeight) {
		if (!videoElement) return;

		const dpr = useCssScaling ? 1 : (window.devicePixelRatio || 1);
		const logicalWidth = roundDownToEven(targetWidth * dpr);
		const logicalHeight = roundDownToEven(targetHeight * dpr);
		console.log(`resetToWinRes logicalWidth: ${logicalWidth} logicalHeight: ${logicalHeight}`)
		if (videoElement.width !== logicalWidth || videoElement.height !== logicalHeight) {
			videoElement.width = logicalWidth;
			videoElement.height = logicalHeight;
			console.log(`Video Element set to: ${logicalWidth}x${logicalHeight}`);
		}

		videoElement.style.position = 'absolute';
		videoElement.style.width = `${logicalWidth}px`;
		videoElement.style.height = `${logicalHeight}px`;
		videoElement.style.top = '0px';
		videoElement.style.left = '0px';
		videoElement.style.objectFit = 'fill';
		console.log(`Resized to window resolution: ${logicalWidth}x${logicalHeight}`);
	}

	function sendResolutionToServer(width, height) {
		const dpr = useCssScaling ? 1 : (window.devicePixelRatio || 1);
		const realWidth = roundDownToEven(width * dpr);
		const realHeight = roundDownToEven(height * dpr);
		const resString = `${realWidth}x${realHeight}`;
		console.log(`Sending resolution to server: ${resString}, Pixel Ratio Used: ${dpr}, useCssScaling: ${useCssScaling}`);
		webrtc.sendDataChannelMessage(`r,${resString}`);
	}

	function enableAutoResize() {
		window.addEventListener("resize", resizeStart);
	}

	function disableAutoResize() {
		window.removeEventListener("resize", resizeStart);
	}

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

	function loadLastSessionSettings() {
		// Preset the video element to last session resolution
		if (window.isManualResolutionMode && manualWidth && manualHeight) {
			console.log(`Applying manual resolution: ${manualWidth}x${manualHeight}`);
			applyManualStyle(manualWidth, manualHeight, scaleLocal);
		} else {
			console.log("Applying window resolution");
			// If manual resolution is not set, reset to window resolution
			const currentWindowRes = input.getWindowResolution();
			resetToWindowResolution(...currentWindowRes);
			sendResolutionToServer(currentWindowRes[0], currentWindowRes[1]);
			enableAutoResize();
		}
	}

	// callback invoked when "message" event is triggerd
	function handleMessage(event) {
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
				setIntParam('manualWidth', null);
				setIntParam('manualHeight', null);
				setBoolParam('isManualResolutionMode', false);
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
				setIntParam('manualWidth', manualWidth);
				setIntParam('manualHeight', manualHeight);
				setBoolParam('isManualResolutionMode', true);
				window.isManualResolutionMode = true;
				break;
			case "setUseCssScaling":
				// TODO: fix issues with hiDPI especially from andriod clients
				// useCssScaling = message.value;
				// setBoolParam('useCssScaling', useCssScaling);
				// console.log(`Set useCssScaling to ${useCssScaling} and persisted.`);

				// input.updateCssScaling();
				// updateVideoImageRendering();
				// if (window.isManualResolutionMode && manualWidth != null && manualHeight != null) {
				//     sendResolutionToServer(manualWidth, manualHeight);
				//     applyManualStyle(manualWidth, manualHeight, scaleLocal);
				// } else {
				//     const currentWindowRes = input.getWindowResolution()
				//     const autoWidth = roundDownToEven(currentWindowRes[0]);
				//     const autoHeight = roundDownToEven(currentWindowRes[1]);
				//     sendResolutionToServer(autoWidth, autoHeight);
				//     resetToWindowResolution(autoWidth, autoHeight)
				// }
				console.warn("Skipping cssScaling since hidpi needs to be implemented")
				break;
			case "clipboardUpdateFromUI":
				console.log("Received clipboard from UI, sending it to server");
				webrtc.sendDataChannelMessage(`cw,${stringToBase64(message.text)}`);
				break;
			case "settings":
				console.log("Received settings msg from dashboard:", message.settings);
				handleSettingsMessage(message.settings);
				break;
			case "command":
				if (message.value !== null || message.value !== undefined) {
					const commandString = message.value;
					console.log(`Received 'command' message with value: "${commandString}"`);
					webrtc.sendDataChannelMessage(`cmd,${commandString}`);
				} else {
					console.warn(`Received invalid command from dashboard: ${message.value}`)
				}
				break;
		}
	}

	// Sends the messages to server and settings are stored in memory after receiving
	// the acknowledgement from server. Check webrtc.onsystemaction() below
	function handleSettingsMessage(settings) {
		if (settings.videoBitRate !== undefined) {
			videoBitRate = parseInt(settings.videoBitRate);
			webrtc.sendDataChannelMessage(`vb,${videoBitRate}`);
			setIntParam('videoBitRate', videoBitRate);
		}
		if (settings.videoFramerate !== undefined) {
			videoFramerate = parseInt(settings.videoFramerate);
			webrtc.sendDataChannelMessage(`_arg_fps,${videoFramerate}`);
			setIntParam('videoFramerate', videoFramerate);
		}
		if (settings.audioBitRate !== undefined) {
			audioBitRate = parseInt(settings.audioBitRate);
			webrtc.sendDataChannelMessage(`ab,${audioBitRate}`);
			setIntParam('audioBitRate', audioBitRate);
		}
		if (settings.encoder !== undefined) {
			console.log("Received encoder setting from dashboard:", settings.encoder);
			encoder = settings.encoder;
			console.warn("Changing of encoder on the fly is not yet supported");
			// setIntParam('encoderRTC', selectedEncoder);
		}
		if (settings.SCALING_DPI !== undefined) {
			const dpi = parseInt(settings.SCALING_DPI, 10);
			webrtc.sendDataChannelMessage(`s,${dpi}`)
		}
	}

	function handleRequestFileUpload() {
		const hiddenInput = document.getElementById('globalFileInput');
		if (!hiddenInput) {
			console.error("Global file input not found!");
			return;
		}
		console.log("Triggering click on hidden file input.");
		hiddenInput.click();
	}

	async function handleFileInputChange(event) {
		const files = event.target.files;
		if (!files || files.length === 0) {
			event.target.value = null;
			return;
		}
		// For every user action 'upload' an auxiliary data is dynamically created.
		// Currently only one aux channel is allowed to operate at a given time, since the backend
		// doesn't support simultaneous reception of multiple files, yet.
		if (!webrtc.createAuxDataChannel()) {
			console.warn("Simultaneous uploading of files with distinct upload operations is not supported yet");
			const errorMsg = "Please let the ongoing upload complete";
			window.postMessage({
				type: 'fileUpload',
				payload: {
				status: 'warning',
				fileName: '_N/A_',
				message: errorMsg
				}
			}, window.location.origin);
			event.target.value = null;
			return;
		}
		console.log(`File input changed, processing ${files.length} files sequentially.`);
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
		} finally {
			event.target.value = null;
			webrtc.closeAuxDataChannel();
		}
	}

	function uploadFileObject(file, pathToSend) {
		return new Promise((resolve, reject) => {
			window.postMessage({
				type: 'fileUpload',
				payload: {
				status: 'start',
				fileName: pathToSend,
				fileSize: file.size
				}
			}, window.location.origin);
			webrtc.sendDataChannelMessage(`FILE_UPLOAD_START:${pathToSend}:${file.size}`)

			let offset = 0;
			const reader = new FileReader();
			reader.onload = async function(e) {
				if (e.target.error) {
					const readErrorMsg = `File read error for ${pathToSend}: ${e.target.error}`;
					window.postMessage({ type: 'fileUpload', payload: { status: 'error', fileName: pathToSend, message: readErrorMsg }}, window.location.origin);
					webrtc.sendDataChannelMessage(`FILE_UPLOAD_ERROR:${pathToSend}:File read error`)
					reject(e.target.error);
					return;
				}
				try {
					const prefixedView = new Uint8Array(1 + e.target.result.byteLength);
					prefixedView[0] = 0x01; // Data prefix for file chunk
					prefixedView.set(new Uint8Array(e.target.result), 1);
					webrtc.sendAuxChannelData(prefixedView.buffer);  // Using auxiliary data channel to send file data
					offset += e.target.result.byteLength;
					const progress = file.size > 0 ? Math.round((offset / file.size) * 100) : 100;
					window.postMessage({
						type: 'fileUpload',
						payload: {
						status: 'progress',
						fileName: pathToSend,
						progress: progress,
						fileSize: file.size
							}
					}, window.location.origin);
					if (offset < file.size) {
						if(webrtc.isAuxBufferNearThreshold()) {
							setTimeout(() => readChunk(offset), 50);
						} else {
							readChunk(offset)
						}
					} else {
						// Data channels work asynchronously due to their underlying implementation,
						// so we need to wait for its buffer to drain before sending the end message.
						await webrtc.awaitForAuxBufferToDrain();
						webrtc.sendDataChannelMessage(`FILE_UPLOAD_END:${pathToSend}`);
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
				} catch (error) {
					const sendErrorMsg = `error during upload of ${pathToSend}: ${error.message || error}`;
					window.postMessage({ type: 'fileUpload', payload: { status: 'error', fileName: pathToSend, message: sendErrorMsg }}, window.location.origin);
					webrtc.sendDataChannelMessage(`FILE_UPLOAD_ERROR:${pathToSend}:send error`);
					reject(error);
				}
			};
			reader.onerror = function(e) {
				const generalReadError = `General file reader error for ${pathToSend}: ${e.target.error}`;
				window.postMessage({ type: 'fileUpload', payload: { status: 'error', fileName: pathToSend, message: generalReadError }}, window.location.origin);
				webrtc.sendDataChannelMessage(`FILE_UPLOAD_ERROR:${pathToSend}:General file reader error`)
				reject(e.target.error);
			};

			function readChunk(startOffset) {
				const slice = file.slice(startOffset, Math.min(startOffset + UPLOAD_CHUNK_SIZE, file.size));
				reader.readAsArrayBuffer(slice);
			}
			readChunk(0);
		});
	}

	function handleDragOver(ev) {
		ev.preventDefault();
		ev.dataTransfer.dropEffect = 'copy';
	}

	async function handleDrop(ev) {
		ev.preventDefault();
		ev.stopPropagation();
		const entriesToProcess = [];
		if (!webrtc.createAuxDataChannel()) {
			console.warn("Simultaneous uploading of files with distinct upload operations is not supported yet");
			const errorMsg = "Please let the ongoing upload complete";
			window.postMessage({
				type: 'fileUpload',
				payload: {
				status: 'warning',
				fileName: '_N/A_',
				message: errorMsg
				}
			}, window.location.origin);
			return;
		}
		if (ev.dataTransfer.items) {
			for (let i = 0; i < ev.dataTransfer.items.length; i++) {
				const entry = ev.dataTransfer.items[i].webkitGetAsEntry() || ev.dataTransfer.items[i].getAsEntry();
				if (entry) entriesToProcess.push(entry);
			}
		} else if (ev.dataTransfer.files.length > 0) {
			for (let i = 0; i < ev.dataTransfer.files.length; i++) {
				await uploadFileObject(ev.dataTransfer.files[i], ev.dataTransfer.files[i].name);
			}
			webrtc.closeAuxDataChannel();
			return;
		}

		// Process the nested entries
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
			webrtc.sendDataChannelMessage(`FILE_UPLOAD_ERROR:GENERAL:Processing failed`)
		}
		webrtc.closeAuxDataChannel();
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
				const file = await getFileFromEntry(entry);
				await uploadFileObject(file, pathToSend);
			} catch (err) {
				console.error(`Error processing file ${pathToSend}: ${err}`);
				window.postMessage({
				type: 'fileUpload',
				payload: { status: 'error', fileName: pathToSend, message: `Error processing file: ${err.message || err}` }
				}, window.location.origin);
				webrtc.sendDataChannelMessage(`FILE_UPLOAD_ERROR:${pathToSend}:Client-side file processing error`)
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

	// TODO: How do we want to render rudimentary metrics?
	function enableStatWatch() {
		// Start watching stats
		var videoBytesReceivedStart = 0;
		var audioBytesReceivedStart = 0;
		var previousVideoJitterBufferDelay = 0.0;
		var previousVideoJitterBufferEmittedCount = 0;
		var previousAudioJitterBufferDelay = 0.0;
		var previousAudioJitterBufferEmittedCount = 0;
		var statsStart = new Date().getTime() / 1000;
		var statsLoop = setInterval(async () => {
			webrtc.getConnectionStats().then((stats) => {
				statWatchEnabled = true;
				var now = new Date().getTime() / 1000;
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

				// Latency stats
				connectionStat.connectionVideoLatency = parseInt(Math.round(rtt + (1000.0 * (stats.video.jitterBufferDelay - previousVideoJitterBufferDelay) / (stats.video.jitterBufferEmittedCount - previousVideoJitterBufferEmittedCount) || 0)));
				previousVideoJitterBufferDelay = stats.video.jitterBufferDelay;
				previousVideoJitterBufferEmittedCount = stats.video.jitterBufferEmittedCount;
				connectionStat.connectionAudioLatency = parseInt(Math.round(rtt + (1000.0 * (stats.audio.jitterBufferDelay - previousAudioJitterBufferDelay) / (stats.audio.jitterBufferEmittedCount - previousAudioJitterBufferEmittedCount) || 0)));
				previousAudioJitterBufferDelay = stats.audio.jitterBufferDelay;
				previousAudioJitterBufferEmittedCount = stats.audio.jitterBufferEmittedCount;

				// Format latency
				connectionStat.connectionLatency =  Math.max(connectionStat.connectionVideoLatency, connectionStat.connectionAudioLatency);

				statsStart = now;
				window.fps = connectionStat.connectionFrameRate

				webrtc.sendDataChannelMessage(`_stats_video,${JSON.stringify(stats.allReports)}`);
			});
		// Stats refresh interval (1000 ms)
		}, 1000);
	}

	function hanldeWindowFocus() {
		// reset keyboard to avoid stuck keys.
		webrtc.sendDataChannelMessage("kr");
		// clipboard interface is only available in secure context
		if (window.isSecureContext) {
			// Send clipboard contents.
			navigator.clipboard.readText()
				.then(text => {
						webrtc.sendDataChannelMessage("cw," + stringToBase64(text))
				})
				.catch(err => {
						webrtc._setStatus('Failed to read clipboard contents: ' + err);
				});
		}
	}

	function handleWindowBlur() {
		// reset keyboard to avoid stuck keys.
		webrtc.sendDataChannelMessage("kr");
	}

	function setupKeyBoardAssisstant() {
		const keyboardInputAssist = document.getElementById('keyboard-input-assist');
		if (keyboardInputAssist && input) {
			keyboardInputAssist.addEventListener('input', (event) => {
				const typedString = keyboardInputAssist.value;
				if (typedString) {
				input._typeString(typedString);
				keyboardInputAssist.value = '';
				}
			});
		keyboardInputAssist.addEventListener('keydown', (event) => {
			if (event.key === 'Enter' || event.keyCode === 13) {
			const enterKeysym = 0xFF0D;
			input._guac_press(enterKeysym);
			setTimeout(() => input._guac_release(enterKeysym), 5);
			event.preventDefault();
			keyboardInputAssist.value = '';
			} else if (event.key === 'Backspace' || event.keyCode === 8) {
			const backspaceKeysym = 0xFF08;
			input._guac_press(backspaceKeysym);
			setTimeout(() => input._guac_release(backspaceKeysym), 5);
			event.preventDefault();
			}
		});
		console.log("Added 'input' and 'keydown' listeners to #keyboard-input-assist.");
		} else {
		console.error(" Could not add listeners to keyboard assist: Element or Input handler instance not found.");
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

			let overlayInput = document.createElement('input');
			overlayInput.type = 'text';
			overlayInput.readOnly = true;
			overlayInput.id = 'overlayInput';

			// prepare the video and audio elements
			videoElement = document.createElement('video');
			videoElement.id = 'stream';
			videoElement.className = 'video';
			videoElement.autoplay = true;
			videoElement.playsInline = true;
			videoElement.contentEditable = 'true';

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
			// Fetch locally stored application data
			appName = window.location.pathname.endsWith("/") && (window.location.pathname.split("/")[1]) || "webrtc";
			debug = getBoolParam('debug', false);
			setBoolParam('debug', debug);
			turnSwitch = getBoolParam('turnSwitch', false);
			setBoolParam('turnSwitch', turnSwitch);
			resizeRemote = getBoolParam('resizeRemote', resizeRemote);
			setBoolParam('resizeRemote', resizeRemote)
			scaleLocal = getBoolParam('scaleLocallyManual', !resizeRemote);
			setBoolParam('scaleLocallyManual', scaleLocal);
			videoBitRate = getIntParam('videoBitRate', videoBitRate);
			setIntParam('videoBitRate', videoBitRate);
			videoFramerate = getIntParam('videoFramerate', videoFramerate);
			setIntParam('videoFramerate', videoFramerate);
			audioBitRate = getIntParam('audioBitRate', audioBitRate);
			setIntParam('audioBitRate', audioBitRate);
			window.isManualResolutionMode = getBoolParam('isManualResolutionMode', false);
			setBoolParam('isManualResolutionMode', window.isManualResolutionMode);
			isGamepadEnabled = getBoolParam('isGamepadEnabled', true);
			setBoolParam('isGamepadEnabled', isGamepadEnabled);
			manualWidth = getIntParam('manualWidth', null);
			setIntParam('manualWidth', manualWidth);
			manualHeight = getIntParam('manualHeight', null);
			setIntParam('manualHeight', manualHeight);
			encoder = getStringParam('encoderRTC', 'x264enc');
			setStringParam('encoderRTC', encoder)
			useCssScaling = getBoolParam('useCssScaling', true);  // TODO: need to handle hiDPI
			setBoolParam('useCssScaling', useCssScaling);

			// listen for dashboard messages (Dashboard -> core client)
			window.addEventListener("message", handleMessage);
			// listen for file upload event
			window.addEventListener('requestFileUpload', handleRequestFileUpload);
			// handlers to handle the drop in files/directories for upload
			overlayInput.addEventListener('dragover', handleDragOver);
			overlayInput.addEventListener('drop', handleDrop);

			// WebRTC entrypoint, connect to the signaling server
			var pathname = window.location.pathname;
			pathname = pathname.slice(0, pathname.lastIndexOf("/") + 1);
			var protocol = (location.protocol == "http:" ? "ws://" : "wss://");
			var signaling = new WebRTCDemoSignaling(new URL(protocol + window.location.host + pathname + appName + "/signaling/"));
			webrtc = new WebRTCDemo(signaling, videoElement, 1);
			const send = (data) => {
				webrtc.sendDataChannelMessage(data);
			}
			input = new Input(overlayInput, send, false, useCssScaling=useCssScaling);

			setupKeyBoardAssisstant();

			// assign the handlers to respective objects
			// TODO: Need to handle the logEntries and DebugEntries list
			signaling.onstatus = (message) => {
				logEntries.push(applyTimestamp("[signaling] " + message));
				console.log("[signaling] " + message);
			};
			signaling.onerror = (message) => {
				logEntries.push(applyTimestamp("[signaling] [ERROR] " + message))
				console.log("[signaling ERROR] " + message);
			};

			signaling.ondisconnect = () => {
				status = 'connecting';
				videoElement.style.cursor = "auto";
				webrtc.reset();
			}

			// Send webrtc status and error messages to logs.
			webrtc.onstatus = (message) => {
				logEntries.push(applyTimestamp("[webrtc] " + message));
				console.log("[webrtc] " + message);
			};
			webrtc.onerror = (message) => {
				logEntries.push(applyTimestamp("[webrtc] [ERROR] " + message));
				console.log("[webrtc] [ERROR] " + message);
			};

			if (debug) {
				signaling.ondebug = (message) => { debugEntries.push("[signaling] " + message); };
				webrtc.ondebug = (message) => { debugEntries.push(applyTimestamp("[webrtc] " + message)) };
			}

			webrtc.ongpustats = async (stats) => {
				// Gpu stats for the Dashboard to render
				window.gpu_stats = stats;
			}

			webrtc.onconnectionstatechange = (state) => {
				videoConnected = state;
				if (videoConnected === "connected") {
					// Repeatedly emit minimum latency target
					webrtc.peerConnection.getReceivers().forEach((receiver) => {
						let intervalLoop = setInterval(async () => {
							if (receiver.track.readyState !== "live" || receiver.transport.state !== "connected") {
								clearInterval(intervalLoop);
								return;
							} else {
								receiver.jitterBufferTarget = receiver.jitterBufferDelayHint = receiver.playoutDelayHint = 0;
							}
						}, 15);
					});
					status = state;
					if (!statWatchEnabled) {
						enableStatWatch();
					}
				}
				updateStatusDisplay();
			};

			webrtc.ondatachannelopen = () => {
				console.log("Data channel opened");
				// Bind gamepad connected handler.
				input.ongamepadconnected = (gamepad_id) => {
					webrtc._setStatus('Gamepad connected: ' + gamepad_id);
					gamepad = {gamepadState: "connected", gamepadName: gamepad_id};
				}

				// Bind gamepad disconnect handler.
				input.ongamepaddisconnected = () => {
					webrtc._setStatus('Gamepad disconnected: ' + gamepad_id);
					gamepad = {gamepadState: "disconnected", gamepadName: "none"};
				}

				// Bind input handlers.
				input.attach();
				loadLastSessionSettings();

				// Send client-side metrics over data channel every 5 seconds
				setInterval(async () => {
					if (connectionStat.connectionFrameRate === parseInt(connectionStat.connectionFrameRate, 10))webrtc.sendDataChannelMessage(`_f,${connectionStat.connectionFrameRate}`);
					if (connectionStat.connectionLatency === parseInt(connectionStat.connectionLatency, 10)) webrtc.sendDataChannelMessage(`_l,${connectionStat.connectionLatency}`);
				}, 5000)
			}

			webrtc.ondatachannelclose = () => {
				input.detach();
			}

			input.onmenuhotkey = () => {
				showDrawer = !showDrawer;
			}

			webrtc.onplaystreamrequired = () => {
				showStart = true;
			}

			// Actions to take whenever window changes focus
			window.addEventListener('focus', hanldeWindowFocus);
			window.addEventListener('blur', handleWindowBlur);

			webrtc.onclipboardcontent = (content) => {
				if (clipboardStatus === 'enabled') {
					navigator.clipboard.writeText(content)
						.catch(err => {
								webrtc._setStatus('Could not copy text to clipboard: ' + err);
					});

					// send the clipboard content to the dashboard interface
					window.postMessage({
						type: 'clipboardContentUpdate',
						text: content
					}, window.location.origin);
				}
			}

			webrtc.oncursorchange = (cursorData) => {
				input.updateServerCursor(cursorData);
			}

			webrtc.onsystemaction = (action) => {
				webrtc._setStatus("Executing system action: " + action);
				if (action === 'reload') {
					setTimeout(() => {
						// trigger webrtc.reset() by disconnecting from the signaling server.
						signaling.disconnect();
					}, 700);
				} else if (action.startsWith('videoFramerate')) {
					// Server received framerate setting.
					const framerateSetting = getIntParam("videoFramerate", null);
					if (framerateSetting !== null) {
						videoFramerate = framerateSetting;
					} else {
						// Use the server setting.
						videoFramerate = parseInt(action.split(",")[1]);
					}
					setIntParam('videoFramerate', videoFramerate);
				} else if (action.startsWith('video_bitrate')) {
					// Server received video bitrate setting.
					const videoBitrateSetting = getIntParam("videoBitRate", null);
					if (videoBitrateSetting !== null) {
						// Prefer the user saved value.
						videoBitRate = videoBitrateSetting;
					} else {
						// Use the server setting.
						videoBitRate = parseInt(action.split(",")[1]);
					}
					setIntParam('videoBitRate', videoBitRate);
				} else if (action.startsWith('audio_bitrate')) {
					// Server received audio bitrate setting.
					const audioBitrateSetting = getIntParam("audioBitRate", null);
					if (audioBitrateSetting !== null) {
						// Prefer the user saved value.
						audioBitRate = audioBitrateSetting
					} else {
						// Use the server setting.
						audioBitRate = parseInt(action.split(",")[1]);
					}
					setIntParam('audioBitRate', audioBitRate);
				} else if (action.startsWith('resize')) {
					// Remote resize enabled/disabled action.
					const resizeSetting = getBoolParam("resize", null);
					if (resizeSetting !== null) {
						// Prefer the user saved value.
						resizeRemote = resizeSetting;
					} else {
						// Use server setting.
						resizeRemote = (action.split(",")[1].toLowerCase() === 'true');
						if (resizeRemote === false && getBoolParam("scaleLocallyManual", null) === null) {
							// Enable local scaling if remote resize is disabled and there is no saved value.
							scaleLocal = true;
						}
					}
				} else {
					webrtc._setStatus('Unhandled system action: ' + action);
				}
			}

			webrtc.onlatencymeasurement = (latency_ms) => {
				serverLatency = latency_ms * 2.0;
			}

			webrtc.onsystemstats = async (stats) => {
				// Dashboard takes care of data validation
				window.system_stats = stats;
			}

			// Safari without Permission API enabled fails
			if (navigator.permissions) {
				navigator.permissions.query({
					name: 'clipboard-read'
				}).then(permissionStatus => {
					// Will be 'granted', 'denied' or 'prompt':
					if (permissionStatus.state === 'granted') {
							clipboardStatus = 'enabled';
					}

					// Listen for changes to the permission state
					permissionStatus.onchange = () => {
							if (permissionStatus.state === 'granted') {
									clipboardStatus = 'enabled';
							}
					};
				});
			}

			// Fetch RTC configuration containing STUN/TURN servers.
			fetch("./turn")
				.then(function (response) {
					return response.json();
				})
				.then((config) => {
					// for debugging, force use of relay server.
					webrtc.forceTurn = turnSwitch;

					// get initial local resolution
					windowResolution = input.getWindowResolution();
					signaling.currRes = windowResolution;

					if (scaleLocal === false) {
							webrtc.element.style.width = windowResolution[0]/window.devicePixelRatio+'px';
							webrtc.element.style.height = windowResolution[1]/window.devicePixelRatio+'px';
					}

					if (config.iceServers.length > 1) {
							debugEntries.push(applyTimestamp("using TURN servers: " + config.iceServers[1].urls.join(", ")));
					} else {
							debugEntries.push(applyTimestamp("no TURN servers found."));
					}
					webrtc.rtcPeerConfig = config;
					webrtc.connect();
				});
		},
		cleanup() {
			// reset the data
			window.isManualResolutionMode = false;
			window.fps = 0;

			// remove the listeners
			window.removeEventListener("message", handleMessage);
			window.removeEventListener("resize", resizeStart);
			window.removeEventListener("requestFileUpload", handleRequestFileUpload);
			window.removeEventListener("focus", hanldeWindowFocus);
			window.removeEventListener("blur", handleWindowBlur);

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
			windowResolution = "";
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
			manualWidth, manualHeight = 0;
			isGamepadEnabled = false;
			videoConnected = "";
			audioConnected = "";
			statWatchEnabled = false;
			webrtc = null;
			input = null;
			useCssScaling = false;
		}
	}
}