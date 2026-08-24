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

/*global GamepadManager, Input*/

/*eslint no-unused-vars: ["error", { "vars": "local" }]*/

/**
 * Peer-connection side of the WebRTC transport.
 *
 * The server offers and the client answers. The offer arrives through
 * lib/signaling.js; the answer is munged before it becomes the local
 * description (`sps-pps-idr-in-keyframe=1` on the H.264 line, and on the
 * Opus line `stereo=1` plus a `minptime` matching the server's `a=ptime`,
 * which is how audio frames shorter than 10 ms get through) and ICE
 * candidates are exchanged the same way, non-relay ones dropped when
 * `forceTurn` is set. The server's video and audio arrive as media tracks
 * on the given element; the audio and video m-lines it offers recvonly are
 * reserved as sendonly transceivers for the microphone and the webcam, which
 * are attached later with `replaceTrack` and no renegotiation.
 *
 * Everything else rides the one data channel the server creates: input and
 * control upstream through `sendDataChannelMessage`, JSON messages
 * downstream, routed by `type` to the `on*` callbacks (`pipeline`,
 * `gpu_stats`, `system_stats`, `cursor`, `system`, `ping`,
 * `latency_measurement`, `server_settings`, `display_config_update` and
 * `clipboard-msg*`). Either side may gzip a message once the `_gz,1`
 * handshake has been exchanged; the multipart clipboard and
 * `server_settings` kinds keep their arrival order across asynchronous
 * inflation, the rest route as soon as they are readable.
 * @module
 */

import { Input } from "./input";

/**
 * WebRTC client: one peer connection plus its data channel.
 *
 * Callbacks are assigned as properties: `onstatus`, `ondebug` and `onerror`
 * receive messages, `onconnectionstatechange` the peer connection state,
 * `ondatachannelopen` and `ondatachannelclose` nothing, `onplaystreamrequired`
 * fires when autoplay was refused and a user gesture is needed, and
 * `onclipboardcontent`, `oncursorchange`, `onsystemaction`, `ongpustats`,
 * `onsystemstats`, `onlatencymeasurement`, `onserversettings` and
 * `ondisplayconfig` receive the payload of the data channel message of the
 * same kind.
 */
export class WebRTCClient {
	/**
	 * @param {WebRTCSignaling} signaling Signaling connection; its `onsdp` and `onice` are taken over.
	 * @param {HTMLVideoElement} element Element the server's stream plays in.
	 * @param {number} peer_id Local peer id registered with the signaling server.
	 */
	constructor(signaling, element, peer_id) {
		/** @type {WebRTCSignaling} */
		this.signaling = signaling;

		/** @type {HTMLVideoElement} */
		this.element = element;

		/** @type {number} */
		this.peer_id = peer_id;

		/** Accept only relay ICE candidates and force `iceTransportPolicy` to `relay`. @type {boolean} */
		this.forceTurn = false;

		/** Configuration handed to `RTCPeerConnection`. @type {Object} */
		this.rtcPeerConfig = {
			"lifetimeDuration": "86400s",
			"iceServers": [
				{
					"urls": [
							"stun:stun.l.google.com:19302"
					]
				},
			],
			"blockStatus": "NOT_BLOCKED",
			"iceTransportPolicy": "all"
		};

		/** @type {RTCPeerConnection} */
		this.peerConnection = null;
		/** Sendonly transceiver the server reserved for the microphone, or null. @type {?RTCRtpTransceiver} */
		this._micTransceiver = null;
		/** Active microphone capture, null until the user enables it. @type {?MediaStream} */
		this._micStream = null;
		/** Sendonly transceiver the server reserved for the webcam, or null. @type {?RTCRtpTransceiver} */
		this._webcamTransceiver = null;
		/** Active camera capture, null until the user enables it. @type {?MediaStream} */
		this._webcamStream = null;

		/** @type {?function(string): void} */
		this.onstatus = null;

		/** @type {?function(string): void} */
		this.ondebug = null;

		/** @type {?function(string): void} */
		this.onerror = null;

		/** @type {?function(string): void} */
		this.onconnectionstatechange = null;

		/** @type {?function(): void} */
		this.ondatachannelopen = null;

		/** @type {?function(): void} */
		this.ondatachannelclose = null;

		/** @type {?function(Object): void} */
		this.ongpustats = null;

		/** @type {?function(number): void} */
		this.onlatencymeasurement = null;

		/** @type {?function(): void} */
		this.onplaystreamrequired = null;

		/** May return a promise, which the ordered receive queue awaits. @type {?function(Object): (void|Promise<void>)} */
		this.onclipboardcontent = null;

		/** @type {?function(string): void} */
		this.onsystemaction = null;

		/** @type {?function(Object): void} */
		this.oncursorchange = null;

		/** @type {Map} */
		this.cursor_cache = new Map();

		/** @type {?function(Object): void} */
		this.onsystemstats = null;

		this.signaling.onsdp = this._onSDP.bind(this);
		this.signaling.onice = this._onSignalingICE.bind(this);

		/** @type {boolean} */
		this._connected = false;

		/** @type {RTCDataChannel} */
		this._send_channel = null;
		/** Whether the server accepted gzip for the upstream direction. @type {boolean} */
		this._gzTx = false;
		/** Order-preserving chain of pending sends around asynchronous compression. @type {Promise<void>} */
		this._sendQueue = Promise.resolve();
		/** Order-preserving chain of pending order-sensitive receives. @type {Promise<void>} */
		this._recvQueue = Promise.resolve();

		/** @type {Input} */
		this.input = null;

		/** @type {Array} */
		this.clipboardcontent = [];

		/** @type {?function(Object): void} */
		this.onserversettings = null;

		/** @type {?function(Object): void} */
		this.ondisplayconfig = null;
	}

	/** Forwards a status message to `onstatus`. */
	_setStatus(message) {
		if (this.onstatus !== null) {
			this.onstatus(message);
		}
	}

	/** Forwards a debug message to `ondebug`. */
	_setDebug(message) {
		if (this.ondebug !== null) {
			this.ondebug(message);
		}
	}

	/** Forwards an error message to `onerror`. */
	_setError(message) {
		if (this.onerror !== null) {
			this.onerror(message);
		}
	}

	/** Forwards the peer connection state to `onconnectionstatechange`. */
	_setConnectionState(state) {
		if (this.onconnectionstatechange !== null) {
			this.onconnectionstatechange(state);
		}
	}

	/**
	 * Adds a remote ICE candidate; with `forceTurn` a candidate without a
	 * relay address (one that went around the TURN server) is rejected.
	 * @param {RTCIceCandidate} icecandidate
	 */
	_onSignalingICE(icecandidate) {
		this._setDebug("received ice candidate from signaling server: " + JSON.stringify(icecandidate));
		if (this.forceTurn && JSON.stringify(icecandidate).indexOf("relay") < 0) {
			this._setDebug("Rejecting non-relay ICE candidate: " + JSON.stringify(icecandidate));
			return;
		}
		this.peerConnection.addIceCandidate(icecandidate).catch(this._setError);
	}

	/**
	 * Sends a local ICE candidate to the server; a null candidate marks the
	 * end of gathering.
	 * @param {RTCPeerConnectionIceEvent} event
	 */
	_onPeerICE(event) {
		if (event.candidate === null) {
			this._setStatus("Completed ICE candidates from peer connection");
			return;
		}
		this.signaling.sendICE(event.candidate);
	}

	/**
	 * Answers the server's offer: sets the remote description, reserves the
	 * uplink transceivers, creates the answer, munges it as described in the
	 * module docblock (the munging has to happen before it becomes the local
	 * description) and sends it. A rejected `setLocalDescription` is surfaced
	 * as an error, since swallowing it would stall the session with no answer
	 * ever sent.
	 * @param {RTCSessionDescription} sdp
	 */
	_onSDP(sdp) {
		if (sdp.type != "offer") {
				this._setError("received SDP was not type offer.");
				return;
		}
		console.log("Received remote SDP", sdp);
		this.peerConnection.setRemoteDescription(sdp).then(() => {
			this._setDebug("received SDP offer, creating answer");
			this._prepareUplinkTransceivers(sdp.sdp);
			this.peerConnection.createAnswer()
			.then((local_sdp) => {
				if (!(/[^-]sps-pps-idr-in-keyframe=1[^\d]/gm.test(local_sdp.sdp)) && (/[^-]packetization-mode=/gm.test(local_sdp.sdp))) {
					console.log("Overriding WebRTC SDP to include sps-pps-idr-in-keyframe=1");
					if (/[^-]sps-pps-idr-in-keyframe=\d+/gm.test(local_sdp.sdp)) {
						local_sdp.sdp = local_sdp.sdp.replace(/sps-pps-idr-in-keyframe=\d+/gm, 'sps-pps-idr-in-keyframe=1');
					} else {
						local_sdp.sdp = local_sdp.sdp.replace('packetization-mode=', 'sps-pps-idr-in-keyframe=1;packetization-mode=');
					}
				}
				if (local_sdp.sdp.indexOf('multiopus') === -1) {
					if (!(/[^-]stereo=1[^\d]/gm.test(local_sdp.sdp)) && (/[^-]useinbandfec=/gm.test(local_sdp.sdp))) {
						console.log("Overriding WebRTC SDP to allow stereo audio");
						if (/[^-]stereo=\d+/gm.test(local_sdp.sdp)) {
							local_sdp.sdp = local_sdp.sdp.replace(/stereo=\d+/gm, 'stereo=1');
						} else {
							local_sdp.sdp = local_sdp.sdp.replace('useinbandfec=', 'stereo=1;useinbandfec=');
						}
					}
					const ptimeMatch = sdp.sdp.match(/^a=ptime:(\d+)/m);
					const minptime = Math.max(3, Math.min(10, ptimeMatch ? parseInt(ptimeMatch[1], 10) : 10));
					if (!(new RegExp('[^-]minptime=' + minptime + '[^\\d]', 'gm').test(local_sdp.sdp)) && (/[^-]useinbandfec=/gm.test(local_sdp.sdp))) {
						console.log("Overriding WebRTC SDP to allow low-latency audio packet (minptime=" + minptime + ")");
						if (/[^-]minptime=\d+/gm.test(local_sdp.sdp)) {
							local_sdp.sdp = local_sdp.sdp.replace(/minptime=\d+/gm, 'minptime=' + minptime);
						} else {
							local_sdp.sdp = local_sdp.sdp.replace('useinbandfec=', 'minptime=' + minptime + ';useinbandfec=');
						}
					}
				}
				console.log("Created local SDP", local_sdp);
				this.peerConnection.setLocalDescription(local_sdp).then(() => {
					this._setDebug("Sending SDP answer");
					this.signaling.sendSDP(this.peerConnection.localDescription);
				}).catch((e) => {
					this._setError("Error setting local description: " + e);
				});
			}).catch(() => {
				this._setError("Error creating local SDP");
			});
		}).catch((e) => {
			this._setError('Error setting remote description: ' + e);
		});
	}

	/**
	 * Reserves the uplinks: the audio m-line the server offered recvonly wants
	 * the microphone and the video m-line it offered recvonly wants the webcam
	 * (its own stream is sendonly). The matching transceivers are marked
	 * sendonly so a track can be attached later with `replaceTrack` and no
	 * renegotiation; a withheld m-line leaves the transceiver null.
	 * @param {string} remoteSdp The offer's SDP text.
	 */
	_prepareUplinkTransceivers(remoteSdp) {
		this._micTransceiver = null;
		this._webcamTransceiver = null;
		if (!remoteSdp || !this.peerConnection) return;
		const recvonlyMids = { audio: null, video: null };
		let curMid = null, curKind = null, curRecvonly = false;
		const closeSection = () => {
			if (curKind && curRecvonly && curMid !== null && recvonlyMids[curKind] === null
				&& Object.prototype.hasOwnProperty.call(recvonlyMids, curKind)) {
				recvonlyMids[curKind] = curMid;
			}
		};
		for (const line of remoteSdp.split(/\r?\n/)) {
			if (line.startsWith('m=')) {
				closeSection();
				curKind = line.slice(2).split(' ')[0];
				curMid = null; curRecvonly = false;
			} else if (line.startsWith('a=mid:')) {
				curMid = line.slice(6).trim();
			} else if (line.trim() === 'a=recvonly') {
				curRecvonly = true;
			}
		}
		closeSection();
		const transceivers = this.peerConnection.getTransceivers();
		const reserve = (mid) => {
			if (mid === null) return null;
			const tx = transceivers.find((t) => t.mid === mid);
			if (tx) {
				try { tx.direction = 'sendonly'; } catch (e) {}
			}
			return tx || null;
		};
		this._micTransceiver = reserve(recvonlyMids.audio);
		this._webcamTransceiver = reserve(recvonlyMids.video);
	}

	/**
	 * Enables or disables the microphone: attaches a getUserMedia track to the
	 * reserved sendonly transceiver (the browser encodes Opus over RTP), or
	 * detaches and stops it.
	 * @param {boolean} enabled
	 * @param {?string} deviceId Capture device, the default one when null.
	 * @returns {Promise<boolean>} False when getUserMedia is unavailable.
	 * @throws {Error} When the server withheld the microphone m-line (the
	 *     microphone is disabled server-side); raised before prompting for
	 *     permission so the UI never claims an active mic that streams nothing.
	 */
	async setMicrophone(enabled, deviceId = null) {
		if (enabled) {
			if (!this._micTransceiver) {
				throw new Error('Microphone is disabled on this server.');
			}
			if (this._micStream) return true;
			if (!navigator.mediaDevices || !navigator.mediaDevices.getUserMedia) return false;
			const audio = { channelCount: 1, sampleRate: 24000, echoCancellation: true, noiseSuppression: true, autoGainControl: true };
			if (deviceId) audio.deviceId = { exact: deviceId };
			this._micStream = await navigator.mediaDevices.getUserMedia({
				audio,
				video: false
			});
			const track = this._micStream.getAudioTracks()[0];
			if (this._micTransceiver && this._micTransceiver.sender && track) {
				await this._micTransceiver.sender.replaceTrack(track);
			}
			return true;
		}
		if (this._micTransceiver && this._micTransceiver.sender) {
			try { await this._micTransceiver.sender.replaceTrack(null); } catch (e) {}
		}
		if (this._micStream) {
			this._micStream.getTracks().forEach((t) => t.stop());
			this._micStream = null;
		}
		return true;
	}

	/**
	 * Enables or disables the webcam: attaches a getUserMedia video track to
	 * the reserved sendonly transceiver (the browser encodes H.264 or VP8 over
	 * RTP and the server's virtual camera decodes it), or detaches and stops
	 * it. Disabling also deactivates the sender's encodings, because a null or
	 * ended track alone does not silence every engine (Firefox keeps the
	 * encoder running on it); enabling re-activates them, all without
	 * renegotiation.
	 * @param {boolean} enabled
	 * @param {?string} deviceId Camera, the default one when null.
	 * @param {{width?: number, height?: number, fps?: number}} hints Capture hints.
	 * @returns {Promise<boolean>} False when getUserMedia is unavailable.
	 * @throws {Error} When the server withheld the webcam m-line (the webcam
	 *     is locked off); raised before prompting for permission.
	 */
	async setWebcam(enabled, deviceId = null, { width = 1280, height = 720, fps = 30 } = {}) {
		if (enabled) {
			if (!this._webcamTransceiver) {
				throw new Error('Webcam is disabled on this server.');
			}
			if (this._webcamStream) return true;
			if (!navigator.mediaDevices || !navigator.mediaDevices.getUserMedia) return false;
			const video = { width: { ideal: width }, height: { ideal: height }, frameRate: { ideal: fps } };
			if (deviceId) video.deviceId = { exact: deviceId };
			this._webcamStream = await navigator.mediaDevices.getUserMedia({ video, audio: false });
			const track = this._webcamStream.getVideoTracks()[0];
			if (this._webcamTransceiver && this._webcamTransceiver.sender && track) {
				await this._setSenderActive(this._webcamTransceiver.sender, true);
				await this._webcamTransceiver.sender.replaceTrack(track);
			}
			return true;
		}
		if (this._webcamTransceiver && this._webcamTransceiver.sender) {
			await this._setSenderActive(this._webcamTransceiver.sender, false);
			try { await this._webcamTransceiver.sender.replaceTrack(null); } catch (e) {}
		}
		if (this._webcamStream) {
			this._webcamStream.getTracks().forEach((t) => t.stop());
			this._webcamStream = null;
		}
		return true;
	}

	/**
	 * Pauses or resumes a sender's encodings in place (`RTCRtpSendParameters.active`).
	 * @param {RTCRtpSender} sender
	 * @param {boolean} active
	 */
	async _setSenderActive(sender, active) {
		try {
			const params = sender.getParameters();
			if (!params.encodings || params.encodings.length === 0) return;
			let changed = false;
			params.encodings.forEach((e) => { if (e.active !== active) { e.active = active; changed = true; } });
			if (changed) await sender.setParameters(params);
		} catch (e) {
			console.warn('Sender encoding activation not applied:', e);
		}
	}

	/** The live camera track sent to the server, or null. @type {?MediaStreamTrack} */
	get webcamTrack() {
		return this._webcamStream ? (this._webcamStream.getVideoTracks()[0] || null) : null;
	}

	/** Logs a created local description. */
	_onLocalSDP(local_sdp) {
		this._setDebug("Created local SDP: " + JSON.stringify(local_sdp));
	}

	/**
	 * Records an incoming track; a video track's stream becomes the element's
	 * source and playback starts.
	 * @param {RTCTrackEvent} event
	 */
	_ontrack(event) {
		this._setStatus("Received incoming " + event.track.kind + " stream from peer");
		if (!this.streams) this.streams = [];
		this.streams.push([event.track.kind, event.streams]);
		if (event.track.kind === "video") {
			this.element.srcObject = event.streams[0];
			this.playStream();
		}
	}

	/**
	 * Adopts the data channel the server created; once it opens, gzip is
	 * offered with `_gz,1` when the engine has `CompressionStream`.
	 * @param {RTCDataChannelEvent} event
	 */
	_onPeerdDataChannel(event) {
		this._setStatus("Peer data channel created: " + event.channel.label);

		this._send_channel = event.channel;
		this._send_channel.binaryType = 'arraybuffer';
		this._send_channel.onmessage = this._onPeerDataChannelMessage.bind(this);
		this._send_channel.onopen = () => {
			if (typeof CompressionStream !== 'undefined') {
				this._send_channel.send('_gz,1');
			}
			if (this.ondatachannelopen !== null)
				this.ondatachannelopen();
		};
		this._send_channel.onclose = () => {
			if (this.ondatachannelclose !== null)
				this.ondatachannelclose();
		};
		this._send_channel.onerror = (event) => {
			this._setError(`Unexpected error, data channel closed, ${event.error || 'unknown error'}`);
		}
	}

	/**
	 * Receives a data channel message. A binary message with the gzip magic
	 * inflates concurrently while a slot in the ordered queue reserves its
	 * arrival position, in case it inflates into an order-sensitive kind; a
	 * kind that needs no ordering routes as soon as it is readable rather than
	 * waiting behind the queue. `_gz,1` enables gzip upstream. Each queued
	 * handler is caught on its own link, so one throwing handler (or a
	 * rejected asynchronous clipboard handler) fails alone instead of leaving
	 * the chain rejected and every later message dropped.
	 * @param {MessageEvent} event
	 */
	_onPeerDataChannelMessage(event) {
		if (event.data instanceof ArrayBuffer) {
			const head = new Uint8Array(event.data, 0, Math.min(2, event.data.byteLength));
			if (head[0] === 0x1f && head[1] === 0x8b) {
				const routed = (async () => {
					const text = await new Response(new Blob([event.data]).stream()
						.pipeThrough(new DecompressionStream('gzip'))).text();
					const msg = this._parseDataChannelMessage(text);
					if (msg === null || this._requiresOrderedDelivery(msg)) {
						return msg;
					}
					this._routeDataChannelMessage(msg);
					return null;
				})().catch((e) => {
					this._setError("failed to decompress data channel message: " + e);
					return null;
				});
				this._recvQueue = this._recvQueue.then(async () => {
					const msg = await routed;
					if (msg !== null) return this._routeDataChannelMessage(msg);
				}).catch((e) => this._setError("failed to handle data channel message: " + e));
				return;
			}
			this._setError("unexpected binary data channel message");
			return;
		}
		if (event.data === '_gz,1') {
			this._gzTx = true;
			return;
		}
		const msg = this._parseDataChannelMessage(event.data);
		if (msg === null) {
			return;
		}
		if (!this._requiresOrderedDelivery(msg)) {
			try {
				this._routeDataChannelMessage(msg);
			} catch (e) {
				this._setError("failed to handle data channel message: " + e);
			}
			return;
		}
		this._recvQueue = this._recvQueue.then(() => this._routeDataChannelMessage(msg))
			.catch((e) => this._setError("failed to handle data channel message: " + e));
	}

	/**
	 * Whether a message rides the ordered queue: multipart clipboard sequences
	 * must reassemble in sequence, and `server_settings` snapshots are
	 * last-wins, so a slow-inflating one must not be overtaken by a newer
	 * plain one. Everything else (cursor, ping, stats, system actions) routes
	 * on arrival so a long clipboard decode cannot delay it.
	 * @param {Object} msg
	 * @returns {boolean}
	 */
	_requiresOrderedDelivery(msg) {
		return typeof msg.type === 'string' &&
			(msg.type.startsWith('clipboard-msg') || msg.type === 'server_settings');
	}

	/**
	 * Parses a JSON message, reporting the failure and returning null when it
	 * is not one.
	 * @param {string} data
	 * @returns {?Object}
	 */
	_parseDataChannelMessage(data) {
		var msg;
		try {
			msg = JSON.parse(data);
		} catch (e) {
			if (e instanceof SyntaxError) {
				this._setError("error parsing data channel message as JSON: " + data);
			} else {
				this._setError("failed to parse data channel message: " + data);
			}
			return null;
		}
		this._setDebug("data channel message: " + data);
		return msg;
	}

	/**
	 * Dispatches a parsed message to its callback by `type`. The clipboard
	 * handler's return value is passed back so the ordered receive queue
	 * awaits it and clipboard messages complete in arrival order.
	 * @param {Object} msg
	 */
	_routeDataChannelMessage(msg) {
		if (msg.type === 'pipeline') {
			this._setStatus(msg.data.status);
		} else if (msg.type === 'gpu_stats') {
			if (this.ongpustats !== null) {
					this.ongpustats(msg.data);
			}
		} else if (typeof msg.type === 'string' && msg.type.startsWith('clipboard-msg')) {
			if (typeof this.onclipboardcontent === 'function') {
				return this.onclipboardcontent(msg);
			}
		} else if (msg.type === 'cursor') {
			if (this.oncursorchange !== null && msg.data !== null) {
				let cursorData = {
					curdata: msg.data.curdata,
					width: msg.data.width,
					height: msg.data.height,
					hotx: msg.data.hotx,
					hoty: msg.data.hoty,
					handle: msg.data.handle,
				};
				this._setDebug(`received new cursor contents, ${JSON.stringify(cursorData)}`);
				this.oncursorchange(cursorData)
			}
		} else if (msg.type === 'system') {
			if (msg.data != null && msg.data.action != null) {
				var action = msg.data.action;
				this._setDebug("received system msg, action: " + action);
				if (this.onsystemaction !== null) {
					this.onsystemaction(action);
				}
			}
		} else if (msg.type === 'ping') {
			this._setDebug("received server ping: " + JSON.stringify(msg.data));
			this.sendDataChannelMessage("pong," + new Date().getTime() / 1000);
		} else if (msg.type === 'system_stats') {
			this._setDebug("received systems stats: " + JSON.stringify(msg.data));
			if (this.onsystemstats !== null) {
				this.onsystemstats(msg.data);
			}
		} else if (msg.type === 'latency_measurement') {
			if (this.onlatencymeasurement !== null) {
				this.onlatencymeasurement(msg.data.latency_ms);
			}
		} else if (msg.type === 'server_settings') {
			if (this.onserversettings !== null) {
				this.onserversettings(msg.data);
			}
		} else if (msg.type === 'display_config_update') {
			if (this.ondisplayconfig !== null) {
				this.ondisplayconfig(msg.data);
			}
		} else {
			this._setError("Unhandled message received: " + msg.type);
		}
	}

	/**
	 * Reacts to the peer connection state: `connected` marks the client up,
	 * `disconnected` closes the data channel and unloads the element, `failed`
	 * unloads it.
	 * @param {string} state
	 */
	_handleConnectionStateChange(state) {
		switch (state) {
			case "connected":
				this._setStatus("Connection complete");
				this._connected = true;
				break;

			case "disconnected":
				this._setError("Peer connection disconnected");
				if (this._send_channel !== null && this._send_channel.readyState === 'open') {
						this._send_channel.close();
				}
				this.element.load();
				break;

			case "failed":
				this._setError("Peer connection failed");
				this.element.load();
				break;
			default:
		}
	}

	/**
	 * Outbound queue depth of the data channel; bulk senders (clipboard,
	 * uploads) throttle on this so they cannot starve input and stats on the
	 * same channel.
	 * @returns {number}
	 */
	dataChannelBufferedAmount() {
		return (this._send_channel && this._send_channel.readyState === 'open')
			? this._send_channel.bufferedAmount : 0;
	}

	/**
	 * Whether the data channel can carry a send right now. Bulk senders check
	 * this to report "not connected" instead of dropping into
	 * `sendDataChannelMessage`'s quiet no-op.
	 * @returns {boolean}
	 */
	dataChannelOpen() {
		return this._send_channel !== null && this._send_channel.readyState === 'open';
	}

	/**
	 * Resolves once queued sends (including the asynchronous gzip queue) have
	 * reached the channel and its buffered amount is below `threshold`. Bulk
	 * senders call this between chunks; without it a burst overflows the SCTP
	 * send buffer and Chromium closes the channel with OperationError, killing
	 * the session. It resumes on the `bufferedamountlow` event rather than a
	 * poll: polling lets the buffer drain to empty between chunks, which
	 * collapses throughput, while keeping about `threshold` bytes queued keeps
	 * the pipe full and still yields the channel to input and stats.
	 * @param {number} threshold Bytes.
	 */
	async waitForDataChannelDrain(threshold = 1024 * 1024) {
		if (this._sendQueue) {
			try { await this._sendQueue; } catch (e) { /* queued send failed; proceed */ }
		}
		const ch = this._send_channel;
		if (!ch || ch.readyState !== 'open' || ch.bufferedAmount <= threshold) return;
		ch.bufferedAmountLowThreshold = threshold;
		await new Promise((resolve) => {
			const done = () => { ch.removeEventListener('bufferedamountlow', done); resolve(); };
			ch.addEventListener('bufferedamountlow', done);
			if (ch.readyState !== 'open' || ch.bufferedAmount <= threshold) done();
		});
	}

	/**
	 * Sends a message on the data channel. Nothing is sent while the channel
	 * is not open: periodic senders fire before it opens while connecting, and
	 * reporting that would mask real failures. Without negotiated gzip the
	 * send is synchronous, so the input hot path pays no latency; with it,
	 * strings of 512 bytes or more gzip asynchronously and every send passes
	 * through an order-preserving queue so a later small message cannot
	 * overtake a large one still compressing.
	 * @param {string|ArrayBuffer} message
	 */
	sendDataChannelMessage(message) {
		if (this._send_channel === null || this._send_channel.readyState !== 'open') {
			return;
		}
		if (!this._gzTx) {
			this._send_channel.send(message);
			return;
		}
		if (typeof message === 'string' && message.length >= 512) {
			this._sendQueue = this._sendQueue.then(async () => {
				const buf = await new Response(new Blob([message]).stream()
					.pipeThrough(new CompressionStream('gzip'))).arrayBuffer();
				if (this._send_channel && this._send_channel.readyState === 'open') {
					this._send_channel.send(buf);
				}
			}).catch(() => {});
		} else {
			this._sendQueue = this._sendQueue.then(() => {
				if (this._send_channel && this._send_channel.readyState === 'open') {
					this._send_channel.send(message);
				}
			}).catch(() => {});
		}
	}


	/**
	 * Reports a gamepad disconnect as a status message.
	 * @param {number} gp_num Gamepad slot.
	 */
	onGamepadDisconnect(gp_num) {
		this._setStatus("gamepad: " + gp_num + ", disconnected");
	}

	/**
	 * Collects connection statistics from `getStats()`.
	 *
	 * `general` comes from the transport and candidate-pair reports (its
	 * `connectionType` through the selected pair's remote candidate), `video`
	 * and `audio` from the inbound-rtp reports (their `codecName` through the
	 * linked codec report), and `data` from the data-channel report; the raw
	 * reports are attached as `reports` and `allReports`. The audio section's
	 * NetEQ concealment counters are the RED acceptance metric; Chrome reports
	 * opus+red under the codec name `opus`, so RED presence is confirmed from
	 * the SDP or the packet size, never from `codecName`.
	 * @returns {Promise<Object>}
	 */
	getConnectionStats() {
		var pc = this.peerConnection;
		var connectionDetails = {
			general: {
				bytesReceived: 0,
				bytesSent: 0,
				connectionType: "NA",
				currentRoundTripTime: null,
				availableReceiveBandwidth: 0,
			},

			video: {
				bytesReceived: 0,
				decoder: "NA",
				frameHeight: 0,
				frameWidth: 0,
				framesPerSecond: 0,
				packetsReceived: 0,
				packetsLost: 0,
				codecName: "NA",
				jitterBufferDelay: 0,
				jitterBufferEmittedCount: 0,
			},

			audio: {
				bytesReceived: 0,
				packetsReceived: 0,
				packetsLost: 0,
				codecName: "NA",
				jitterBufferDelay: 0,
				jitterBufferEmittedCount: 0,
				concealedSamples: 0,
				concealmentEvents: 0,
				totalSamplesReceived: 0,
				packetsDiscarded: 0,
			},

			data: {
				bytesReceived: 0,
				bytesSent: 0,
				messagesReceived: 0,
				messagesSent: 0,
			}
		};

		return new Promise(function (resolve, reject) {
			pc.getStats().then((stats) => {
				var reports = {
					transports: {},
					candidatePairs: {},
					selectedCandidatePairId: null,
					remoteCandidates: {},
					codecs: {},
					videoRTP: null,
					videoTrack: null,
					audioRTP: null,
					audioTrack: null,
					dataChannel: null,
				};

				var allReports = [];

				stats.forEach((report) => {
					allReports.push(report);
					if (report.type === "transport") {
						reports.transports[report.id] = report;
					} else if (report.type === "candidate-pair") {
						reports.candidatePairs[report.id] = report;
						if (report.selected === true) {
							reports.selectedCandidatePairId = report.id;
						}
					} else if (report.type === "inbound-rtp") {
						if (report.kind === "video") {
							reports.videoRTP = report;
						} else if (report.kind === "audio") {
							reports.audioRTP = report;
						}
					} else if (report.type === "track") {
						if (report.kind === "video") {
							reports.videoTrack = report;
						} else if (report.kind === "audio") {
							reports.audioTrack = report;
						}
					} else if (report.type === "data-channel") {
						reports.dataChannel = report;
					} else if (report.type === "remote-candidate") {
						reports.remoteCandidates[report.id] = report;
					} else if (report.type === "codec") {
						reports.codecs[report.id] = report;
					}
				});

				var videoRTP = reports.videoRTP;
				if (videoRTP !== null) {
					connectionDetails.video.bytesReceived = videoRTP.bytesReceived;
					// decoderImplementation is only exposed while the media context is in a capturing state.
					connectionDetails.video.decoder = videoRTP.decoderImplementation || "unknown";
					connectionDetails.video.frameHeight = videoRTP.frameHeight;
					connectionDetails.video.frameWidth = videoRTP.frameWidth;
					connectionDetails.video.framesPerSecond = videoRTP.framesPerSecond;
					connectionDetails.video.packetsReceived = videoRTP.packetsReceived;
					connectionDetails.video.packetsLost = videoRTP.packetsLost;

					var codec = reports.codecs[videoRTP.codecId];
					if (codec !== undefined) {
						connectionDetails.video.codecName = codec.mimeType.split("/")[1].toUpperCase();
					}
				}

				var audioRTP = reports.audioRTP;
				if (audioRTP !== null) {
					connectionDetails.audio.bytesReceived = audioRTP.bytesReceived;
					connectionDetails.audio.packetsReceived = audioRTP.packetsReceived;
					connectionDetails.audio.packetsLost = audioRTP.packetsLost;
					if (audioRTP.concealedSamples !== undefined) connectionDetails.audio.concealedSamples = audioRTP.concealedSamples;
					if (audioRTP.concealmentEvents !== undefined) connectionDetails.audio.concealmentEvents = audioRTP.concealmentEvents;
					if (audioRTP.totalSamplesReceived !== undefined) connectionDetails.audio.totalSamplesReceived = audioRTP.totalSamplesReceived;
					if (audioRTP.packetsDiscarded !== undefined) connectionDetails.audio.packetsDiscarded = audioRTP.packetsDiscarded;

					var codec = reports.codecs[audioRTP.codecId];
					if (codec !== undefined) {
						connectionDetails.audio.codecName = codec.mimeType.split("/")[1].toUpperCase();
					}
				}

				var dataChannel = reports.dataChannel;
				if (dataChannel !== null) {
					connectionDetails.data.bytesReceived = dataChannel.bytesReceived;
					connectionDetails.data.bytesSent = dataChannel.bytesSent;
					connectionDetails.data.messagesReceived = dataChannel.messagesReceived;
					connectionDetails.data.messagesSent =  dataChannel.messagesSent;
				}

				if (Object.keys(reports.transports).length > 0) {
					var transport = reports.transports[Object.keys(reports.transports)[0]];
					connectionDetails.general.bytesReceived = transport.bytesReceived;
					connectionDetails.general.bytesSent = transport.bytesSent;
					reports.selectedCandidatePairId = transport.selectedCandidatePairId;
				} else if (reports.selectedCandidatePairId !== null) {
					connectionDetails.general.bytesReceived = reports.candidatePairs[reports.selectedCandidatePairId].bytesReceived;
					connectionDetails.general.bytesSent = reports.candidatePairs[reports.selectedCandidatePairId].bytesSent;
				}

				if (reports.selectedCandidatePairId !== null) {
					var candidatePair = reports.candidatePairs[reports.selectedCandidatePairId];
					if (candidatePair !== undefined) {
						if (candidatePair.availableIncomingBitrate !== undefined) {
							connectionDetails.general.availableReceiveBandwidth = candidatePair.availableIncomingBitrate;
						}
						if (candidatePair.currentRoundTripTime !== undefined) {
							connectionDetails.general.currentRoundTripTime = candidatePair.currentRoundTripTime;
						}
						var remoteCandidate = reports.remoteCandidates[candidatePair.remoteCandidateId];
						if (remoteCandidate !== undefined) {
							connectionDetails.general.connectionType = remoteCandidate.candidateType;
						}
					}
				}

				connectionDetails.general.packetsReceived = connectionDetails.video.packetsReceived + connectionDetails.audio.packetsReceived;
				connectionDetails.general.packetsLost = connectionDetails.video.packetsLost + connectionDetails.audio.packetsLost;

				if (reports.videoRTP !== null) {
					connectionDetails.video.jitterBufferDelay = reports.videoRTP.jitterBufferDelay;
					connectionDetails.video.jitterBufferEmittedCount = reports.videoRTP.jitterBufferEmittedCount;
				}

				if (reports.audioRTP !== null) {
					connectionDetails.audio.jitterBufferDelay = reports.audioRTP.jitterBufferDelay;
					connectionDetails.audio.jitterBufferEmittedCount = reports.audioRTP.jitterBufferEmittedCount;
				}

				connectionDetails.reports = reports;
				connectionDetails.allReports = allReports;

				resolve(connectionDetails);
			}).catch( (e) => reject(e));
		});
	}

	/**
	 * Starts playback of the element. Engines refuse autoplay before a user
	 * gesture; that refusal is reported through `onplaystreamrequired`.
	 */
	playStream() {
		this.element.load();

		var playPromise = this.element.play();
		if (playPromise !== undefined) {
			playPromise.then(() => {
				this._setDebug("Stream is playing.");
			}).catch(() => {
				if (this.onplaystreamrequired !== null) {
					this.onplaystreamrequired();
				} else {
					this._setDebug("Stream play failed and no onplaystreamrequired was bound.");
				}
			});
		}
	}

	/**
	 * Creates the peer connection (relay-only when `forceTurn` is set) and
	 * connects the signaling client, which starts the offer/answer exchange.
	 */
	connect() {
		this.peerConnection = new RTCPeerConnection(this.rtcPeerConfig);
		this.peerConnection.ontrack = this._ontrack.bind(this);
		this.peerConnection.onicecandidate = this._onPeerICE.bind(this);
		this.peerConnection.ondatachannel = this._onPeerdDataChannel.bind(this);

		this.peerConnection.onconnectionstatechange = () => {
			this._handleConnectionStateChange(this.peerConnection.connectionState);
			this._setConnectionState(this.peerConnection.connectionState);
		};

		if (this.forceTurn) {
			this._setStatus("forcing use of TURN server");
			const config = this.peerConnection.getConfiguration();
			config.iceTransportPolicy = "relay";
			this.peerConnection.setConfiguration(config);
		}

		this.signaling.peer_id = this.peer_id;
		this.signaling.connect();
	}

	/**
	 * Resets the connection: forgets the cursor cache, closes the data channel
	 * and the peer connection, and reconnects, after a three-second pause when
	 * signaling was not stable.
	 */
	reset() {
		this.cursor_cache = new Map();

		var signalState = this.peerConnection.signalingState;
		if (this._send_channel !== null && this._send_channel.readyState === "open") {
			this._send_channel.close();
		}
		if (this.peerConnection !== null) this.peerConnection.close();
		if (signalState !== "stable") {
			setTimeout(() => {
					this.connect();
			}, 3000);
		} else {
			this.connect();
		}
	}
}