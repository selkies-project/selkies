/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * What a dashboard shows of the session, gathered in one place for both cores
 * and published on `window` under one contract.
 *
 * `window.stream_info` is what the server said of the stream: the capture path
 * and whether it is zero-copy, the encoder and whether it is hardware, the GPU,
 * and the reason a faster path was declined (`stream_stats.py` on the server).
 * It arrives once and again on a change, whether or not anybody looks.
 * `window.stream_client` is this page's half: the transport and the path it
 * took, the codec and resolution, and the decoder with the evidence for calling
 * it hardware or software.
 *
 * Everything that moves is gathered only while a dashboard has its stats on
 * screen. It says so with a `statsOpen` window message; the collector then asks
 * the server for `stream_stats` (the `_stats` verb), the core samples its own
 * side once a second, and each second's figures land in
 * `window.stream_stats`: `latest`, and `history`, which starts empty when the
 * stats open and grows while they stay open, up to `HISTORY_MAX` seconds. Shut,
 * nothing is sampled, nothing is sent and the server sends nothing. A shared
 * viewer never subscribes.
 *
 * @module
 */

/** Seconds of history kept for the graphs. */
export const HISTORY_MAX = 600;
/** Server figures older than this are left out of a sample, in ms. */
export const SERVER_FRESH_MS = 3000;

/**
 * The stream's latency as the stages that were measured add up: the capture and
 * encode, one way of the round trip, the jitter buffer a WebRTC page holds back,
 * and the decode. It is a sum of measurements, not a glass-to-glass reading --
 * nothing here times a key press to the pixel it changes -- and it is absent
 * until a stage has been measured, so an idle screen reads as a dash rather than
 * as zero.
 * @param {StreamSample} sample
 * @returns {number|undefined}
 */
function latencyOf(sample) {
  let total = 0;
  let measured = false;
  const add = (value) => {
    if (typeof value === 'number' && isFinite(value)) {
      total += value;
      measured = true;
    }
  };
  add(sample.pipeline_ms);
  if (typeof sample.rtt_ms === 'number' && isFinite(sample.rtt_ms)) add(sample.rtt_ms / 2);
  add(sample.jitter_buffer_ms);
  add(sample.decode_ms);
  return measured ? Math.round(total * 10) / 10 : undefined;
}

/**
 * @typedef {Object} StreamInfo The server's description of the stream.
 * @property {string} backend `x11` or `wayland`.
 * @property {string} capture The capture path: `NvFBC`, `DRI3`, `XShm`,
 *     `dmabuf` or `readback`.
 * @property {boolean} zero_copy Whether frames reach the encoder without a copy.
 * @property {string} capture_reason Why not, empty where there is nothing to explain.
 * @property {string} encoder `NVENC`, `VAAPI`, or the software library.
 * @property {boolean} hardware Whether the encoder runs on a GPU.
 * @property {boolean} gpu_present Whether the server is exposed a GPU at all.
 * @property {boolean} hardware_expected Whether the session asked for that on a
 *     server that has one.
 * @property {string} encoder_reason Why it does not.
 * @property {string} codec
 * @property {boolean} fullcolor Whether the stream is 4:4:4.
 * @property {boolean} striped
 * @property {string} gpu The GPU a hardware session encodes on.
 * @property {string} driver Its kernel driver.
 * @property {string} encode_node Its render node.
 * @property {string} [renderer] Wayland only: `gl` or `pixman`.
 * @property {string} [render_node]
 * @property {string} [render_gpu]
 * @property {string} [renderer_reason]
 */

/**
 * @typedef {Object} StreamClient This page's description of the stream.
 * @property {'websockets'|'webrtc'} transport
 * @property {'hardware'|'software'|'unknown'} decoder
 * @property {string} decoder_evidence What that verdict rests on.
 * @property {string} codec
 * @property {string} resolution `1920x1080`.
 * @property {string} path WebRTC only: the candidate pair, as `host udp`.
 */

/**
 * @typedef {Object<string, (number|boolean|string)>} StreamSample One second's
 *     figures: the server's (`cpu_percent`, `mem_used`, `mem_total`,
 *     `gpu_percent`, `gpu_mem_used`, `gpu_mem_total`, `encoded_fps`,
 *     `encode_ms`, `pipeline_ms`, `rtt_ms`, `throttled`) where it sent them, this
 *     page's (`fps`, `mbps`, and whatever else its core measures), and `t`, the
 *     time in ms.
 */

/**
 * What a decoded frame's pixel format says of the decoder behind it. A hardware
 * decoder hands out NV12 or an opaque frame, a software one planar I4xx.
 * @param {string|null|undefined} format `VideoFrame.format`; undefined where no frame was seen.
 * @returns {'hardware'|'software'|'unknown'}
 */
export function decoderOfFormat(format) {
  if (format === undefined) return 'unknown';
  if (format === null || format === 'NV12') return 'hardware';
  return /^I4/.test(format) ? 'software' : 'unknown';
}

/**
 * The WebCodecs decoder verdict from everything the page can know: a software
 * preference is certain, an engine that refuses the stream's configuration with
 * hardware required has none, and otherwise the frames say which kind made them.
 * @param {{forcedSoftware: boolean, hardwareSupported: (boolean|null|undefined),
 *     format: (string|null|undefined)}} evidence `hardwareSupported` is
 *     `VideoDecoder.isConfigSupported` with `prefer-hardware`, null where unasked.
 * @returns {{decoder: ('hardware'|'software'|'unknown'), decoder_evidence: string}}
 */
export function webcodecsDecoder({ forcedSoftware, hardwareSupported, format }) {
  if (forcedSoftware) return { decoder: 'software', decoder_evidence: 'prefer-software after a decoder fallback' };
  if (hardwareSupported === false) return { decoder: 'software', decoder_evidence: 'no hardware decoder for this stream' };
  const seen = decoderOfFormat(format);
  if (seen === 'unknown') return { decoder: 'unknown', decoder_evidence: '' };
  return { decoder: seen, decoder_evidence: `${format === null ? 'opaque' : format} frames` };
}

/**
 * The WebRTC decoder verdict from the inbound video report. Engines name the
 * decoder only to a page that holds a capture permission, so without one the
 * verdict falls back to whether the engine has an efficient decoder at all.
 * @param {{implementation: (string|undefined), powerEfficient: (boolean|undefined),
 *     capable: (boolean|null|undefined)}} evidence `capable` is
 *     `MediaCapabilities.decodingInfo().powerEfficient` for the stream.
 * @returns {{decoder: ('hardware'|'software'|'unknown'), decoder_evidence: string}}
 */
export function webrtcDecoder({ implementation, powerEfficient, capable }) {
  const named = implementation && implementation !== 'unknown' ? implementation : '';
  if (typeof powerEfficient === 'boolean') {
    return { decoder: powerEfficient ? 'hardware' : 'software', decoder_evidence: named };
  }
  if (named) {
    const software = /libvpx|ffmpeg|dav1d|openh264|libaom/i.test(named);
    return { decoder: software ? 'software' : 'hardware', decoder_evidence: named };
  }
  if (capable === true) return { decoder: 'unknown', decoder_evidence: 'a hardware decoder is available' };
  if (capable === false) return { decoder: 'software', decoder_evidence: 'no hardware decoder for this stream' };
  return { decoder: 'unknown', decoder_evidence: '' };
}

export class StreamStats {
  /**
   * @param {{transport: ('websockets'|'webrtc'), send: function(string): void,
   *     isViewer: function(): boolean, onOpenChange: (function(boolean): void|undefined)}} options
   *     `send` puts one text message on the session connection; `onOpenChange`
   *     lets the core start and stop its own sampling.
   */
  constructor({ transport, send, isViewer, onOpenChange }) {
    this._send = send;
    this._isViewer = isViewer;
    this._onOpenChange = onOpenChange || (() => {});
    this._open = false;
    this._subscribed = false;
    /** @type {StreamSample} */
    this._server = {};
    this._serverAt = 0;
    this._bytes = 0;
    this._bytesAt = performance.now();
    /** @type {StreamClient} */
    this._client = { transport, decoder: 'unknown', decoder_evidence: '', codec: '', resolution: '', path: '' };
    window.stream_info = null;
    window.stream_client = this._client;
    window.stream_stats = { open: false, latest: null, history: [] };
  }

  /** Whether a dashboard has its stats on screen. */
  get open() {
    return this._open;
  }

  /**
   * A dashboard opened or shut its stats. Opening starts a fresh history.
   * @param {boolean} open
   */
  setOpen(open) {
    open = !!open;
    if (open === this._open) return;
    this._open = open;
    this._bytes = 0;
    this._bytesAt = performance.now();
    window.stream_stats = { open, latest: null, history: [] };
    this.subscribe();
    this._onOpenChange(open);
  }

  /** Tells the server what this page wants; a fresh connection has to be told again. */
  subscribe() {
    const want = this._open && !this._isViewer();
    if (!want && !this._subscribed) return;
    this._subscribed = want;
    try {
      this._send(`_stats,${want ? 1 : 0}`);
    } catch (_) {
      this._subscribed = false;
    }
  }

  /** The connection went away, and the subscription with it. */
  disconnected() {
    this._subscribed = false;
  }

  /** @param {StreamInfo|null} info The server's `stream_info`. */
  setInfo(info) {
    window.stream_info = info || null;
  }

  /** @param {Partial<StreamClient>} description What the core learned of its own side. */
  setClient(description) {
    Object.assign(this._client, description);
  }

  /** @param {StreamSample} stats The server's `stream_stats`. */
  serverSample(stats) {
    this._server = stats || {};
    this._serverAt = performance.now();
  }

  /** @param {number} bytes Stream bytes that arrived, for the bandwidth figure. */
  noteBytes(bytes) {
    this._bytes += bytes;
  }

  /**
   * One second's figures from the core, merged with the server's and appended
   * to the history. `mbps` comes from `noteBytes` unless the core measured it.
   * @param {StreamSample} figures
   */
  clientSample(figures) {
    if (!this._open) return;
    const now = performance.now();
    const elapsed = (now - this._bytesAt) / 1000;
    const server = now - this._serverAt <= SERVER_FRESH_MS ? this._server : {};
    const sample = Object.assign({ t: Date.now() }, server, figures);
    if (sample.mbps === undefined && elapsed > 0) {
      sample.mbps = Math.round((this._bytes * 8 / 1e6 / elapsed) * 100) / 100;
    }
    const latency = latencyOf(sample);
    if (latency !== undefined) sample.latency_ms = latency;
    this._bytes = 0;
    this._bytesAt = now;
    const stats = window.stream_stats;
    stats.latest = sample;
    stats.history.push(sample);
    if (stats.history.length > HISTORY_MAX) stats.history.shift();
  }
}
