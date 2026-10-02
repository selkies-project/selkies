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
 * it hardware or software. It changes only with the stream, so the core keeps
 * it current whether or not anybody looks, and a dashboard that opens its stats
 * draws it at once.
 *
 * Everything that moves is gathered only while a dashboard has its stats on
 * screen. It says so with a `statsOpen` window message; the collector then asks
 * the server for `stream_stats` (the `_stats` verb), the core samples its own
 * side `FIRST_SAMPLE_MS` after the opening and once a second after that, and
 * each sample lands in `window.stream_stats`: `latest`, and `history`, which
 * starts empty when the stats open and grows while they stay open, up to
 * `HISTORY_MAX` seconds. What a sample counts (data received, frames dropped,
 * repairs asked for) is counted from the opening. Shut, nothing is sampled,
 * nothing is sent, and the server sends nothing. A shared viewer never
 * subscribes. Every change to the three is announced with a `STATS_EVENT`
 * event on `window`, which is when a dashboard reads them.
 *
 * @module
 */

/** Seconds of history kept for the graphs. */
export const HISTORY_MAX = 600;
/** How long after the stats open the first sample is taken, in ms. */
export const FIRST_SAMPLE_MS = 500;
/** The event on `window` that announces a change to what the stats show. */
export const STATS_EVENT = 'selkies-stream-stats';
/** Server figures older than this are left out of a sample, in ms. */
export const SERVER_FRESH_MS = 3000;

/**
 * @typedef {Object} StreamInfo The server's description of the stream.
 * @property {string} backend `x11` or `wayland`.
 * @property {string} capture The capture path: `NvFBC`, `DRI3`, `XShm`,
 *     `dmabuf`, or `readback`.
 * @property {boolean} zero_copy Whether frames reach the encoder without a copy.
 * @property {boolean} zero_copy_available Whether the display server offered the
 *     encoder a zero-copy path at all, so that a copy fell short of one.
 * @property {string} capture_reason Why not, empty where there is nothing to explain.
 * @property {string} encoder `NVENC`, `VAAPI`, or the software library.
 * @property {boolean} hardware Whether the encoder runs on a GPU.
 * @property {boolean} gpu_present Whether the server is exposed a GPU at all.
 * @property {boolean} hardware_expected Whether the session asked for that on a
 *     server that has one.
 * @property {string} encoder_reason Why it does not.
 * @property {string} codec
 * @property {boolean} fullcolor Whether the stream is 4:4:4.
 * @property {number} [bit_depth] Bits per sample of the stream, 8 or 10.
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
 * @property {boolean} hardware_expected Whether a hardware decoder was there to
 *     take the stream, so a software verdict fell short of it.
 * @property {string} decoder_reason Why a software decode fell short, a word
 *     the dashboards translate: `hardware_available` (the engine has an
 *     efficient decoder for the stream) or `software_preferred` (this client
 *     left a hardware decoder after it failed); empty where it did not.
 * @property {string} codec
 * @property {string} resolution `1920x1080`.
 * @property {string} path WebRTC only: the candidate pair, as `host udp`.
 * @property {string} sink How decoded frames reach the screen: a track
 *     generator feeding a `<video>`, a canvas a worker composites on, or the
 *     page's own canvas. Which one an engine allows is probed, so this is the
 *     one that was taken rather than the one preferred.
 * @property {string} decode_path JPEG only: what turns a stripe into a picture,
 *     and where it runs.
 * @property {'ok'|'poor'} connection The server's verdict on this page's
 *     connection: poor while too many of its display's frames are lost on the
 *     way or held back for its link (`ConnectionVerdict` on the server). The
 *     server says so only when it changes, so it is kept with the stats shut.
 */

/**
 * @typedef {Object<string, (number|boolean|string)>} StreamSample One second's
 *     figures: the server's (`cpu_percent`, `mem_used`, `mem_total`,
 *     `gpu_percent`, `gpu_mem_used`, `gpu_mem_total`, `encoded_fps`,
 *     `encode_ms`, `pipeline_ms`, `rtt_ms`, `throttled`) where it sent them, and
 *     `server`, whether it did, this page's (`fps`, the frames the screen was
 *     handed, where the sink says so; `present_ms`, how long a frame took from
 *     its arrival to that; `frames_not_shown` since the opening, those decoded
 *     but never shown; `mbps`, `received_mb` since the opening, and whatever
 *     else its core measures, `lib/present-meter.js` for the three), and `t`,
 *     the time in ms.
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

/** What a decoded frame's pixel format is called in the evidence. */
const framesOf = (format) => `${format === null ? 'Opaque' : format} frames`;

/**
 * A decoder verdict and what it rests on, with whether it fell short: software
 * where a hardware decoder was there to take the stream.
 * @param {'hardware'|'software'|'unknown'} decoder
 * @param {string} evidence
 * @param {boolean} expected Whether a hardware decoder was there.
 * @param {string} [shortfall] Why software fell short; said only where it did.
 * @returns {Pick<StreamClient, 'decoder'|'decoder_evidence'|'hardware_expected'|'decoder_reason'>}
 */
function verdict(decoder, evidence, expected, shortfall = 'hardware_available') {
  return { decoder, decoder_evidence: evidence, hardware_expected: expected,
    decoder_reason: decoder === 'software' && expected ? shortfall : '' };
}

/**
 * The last word where nothing names the decoder: whether the engine has an
 * efficient one for the stream at all (`DecodeCapability`). An engine that has
 * one is taken to be using it, since every sign of a software decode (a
 * preference after a fallback, a refused hardware configuration, planar frames)
 * has been read first and none was there; a verdict of unknown here would leave
 * a working hardware decoder unmarked on the engines whose frames say nothing.
 * @param {boolean|null|undefined} capable
 * @param {string} evidence What little there is otherwise.
 * @returns {Pick<StreamClient, 'decoder'|'decoder_evidence'|'hardware_expected'|'decoder_reason'>}
 */
function capabilityVerdict(capable, evidence) {
  if (capable === true) return verdict('hardware', 'A hardware decoder is available', true);
  if (capable === false) return verdict('software', 'No hardware decoder for this stream', false);
  return verdict('unknown', evidence, false);
}

/**
 * Whether the engine decodes a configuration efficiently, from
 * `MediaCapabilities`: what stands in for the decoder's own word where neither
 * the engine nor its frames say which decoder took the stream. Asked once per
 * configuration; `efficient` is null until the engine answers, and where it
 * cannot.
 */
export class DecodeCapability {
  constructor() {
    this.key = '';
    /** @type {?boolean} */
    this.efficient = null;
  }

  /**
   * @param {MediaDecodingConfiguration} configuration What `decodingInfo` is asked.
   * @param {string} key The configuration the answer is kept for.
   */
  ask(configuration, key) {
    if (key === this.key) return;
    this.key = key;
    this.efficient = null;
    if (!navigator.mediaCapabilities || !navigator.mediaCapabilities.decodingInfo) return;
    navigator.mediaCapabilities.decodingInfo(configuration).then((info) => {
      if (this.key === key) this.efficient = info.supported ? !!info.powerEfficient : null;
    }).catch(() => {});
  }
}

/**
 * The WebCodecs decoder verdict from everything the page can know: a software
 * preference after a fallback is certain and falls short of the hardware it left,
 * an engine that refuses the stream's configuration with hardware preferred has
 * none, the frames say which kind made them where their format tells, and an
 * engine whose frames are all one format whatever decodes them (Gecko hands out
 * BGRX) is left to `capabilityVerdict`, which calls an engine with an efficient
 * decoder hardware. Software falls short, as over WebRTC,
 * where the engine says it decodes this configuration efficiently; that it
 * accepts the hardware preference says nothing, since some engines take it as
 * a hint and decode in software.
 * @param {{forcedSoftware: boolean, hardwareSupported: (boolean|null|undefined),
 *     format: (string|null|undefined), capable: (boolean|null|undefined)}} evidence
 *     `hardwareSupported` is `VideoDecoder.isConfigSupported` with
 *     `prefer-hardware`, null where unasked; `capable` is `DecodeCapability.efficient`.
 * @returns {Pick<StreamClient, 'decoder'|'decoder_evidence'|'hardware_expected'|'decoder_reason'>}
 */
export function webcodecsDecoder({ forcedSoftware, hardwareSupported, format, capable }) {
  if (forcedSoftware) return verdict('software', '', true, 'software_preferred');
  if (hardwareSupported === false) return verdict('software', 'No hardware decoder for this stream', false);
  const seen = decoderOfFormat(format);
  if (seen !== 'unknown') return verdict(seen, framesOf(format), capable === true);
  return capabilityVerdict(capable, format === undefined ? '' : framesOf(format));
}

/**
 * The WebRTC decoder verdict. Engines name the decoder in the inbound video
 * report only to a page that holds a capture permission. Without one, a frame
 * read from the received track is the decoder's own, as the WebCodecs verdict
 * reads it; the element's current frame is not, since an engine may copy a
 * software picture into GPU memory to composite it, so only a planar one says
 * anything (software). Last is whether the engine has an efficient decoder for
 * the stream at all, taken as the one in use where nothing said software.
 * @param {{implementation: (string|undefined), powerEfficient: (boolean|undefined),
 *     trackFormat: (string|null|undefined), elementFormat: (string|null|undefined),
 *     capable: (boolean|null|undefined)}} evidence `trackFormat` is a
 *     `MediaStreamTrackProcessor` frame's format and `elementFormat` a
 *     `VideoFrame` of the `<video>`'s, undefined where none was read;
 *     `capable` is `DecodeCapability.efficient`.
 * @returns {Pick<StreamClient, 'decoder'|'decoder_evidence'|'hardware_expected'|'decoder_reason'>}
 */
export function webrtcDecoder({ implementation, powerEfficient, trackFormat, elementFormat, capable }) {
  const named = implementation && implementation !== 'unknown' ? implementation : '';
  const expected = capable === true;
  if (typeof powerEfficient === 'boolean') return verdict(powerEfficient ? 'hardware' : 'software', named, expected);
  if (named) return verdict(/libvpx|ffmpeg|dav1d|openh264|libaom/i.test(named) ? 'software' : 'hardware', named, expected);
  const fromTrack = decoderOfFormat(trackFormat);
  if (fromTrack !== 'unknown') return verdict(fromTrack, framesOf(trackFormat), expected);
  if (decoderOfFormat(elementFormat) === 'software') return verdict('software', framesOf(elementFormat), expected);
  return capabilityVerdict(capable, '');
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
    this._serverAt = -Infinity;
    this._bytes = 0;
    this._bytesAt = performance.now();
    this._received = 0;
    this._announcing = false;
    /** @type {StreamClient} */
    this._client = { transport, decoder: 'unknown', decoder_evidence: '', hardware_expected: false,
      decoder_reason: '', codec: '', resolution: '', path: '', sink: '', decode_path: '', connection: 'ok' };
    window.stream_info = null;
    window.stream_client = this._client;
    window.stream_stats = { open: false, latest: null, history: [] };
  }

  /** Whether a dashboard has its stats on screen. */
  get open() {
    return this._open;
  }

  /** Announces a change, once for everything that changed in the same task. */
  _changed() {
    if (this._announcing || typeof window.dispatchEvent !== 'function') return;
    this._announcing = true;
    queueMicrotask(() => {
      this._announcing = false;
      window.dispatchEvent(new Event(STATS_EVENT));
    });
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
    this._received = 0;
    window.stream_stats = { open, latest: null, history: [] };
    this.subscribe();
    this._onOpenChange(open);
    this._changed();
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

  /** The connection went away, and the subscription and its verdict with it. */
  disconnected() {
    this._subscribed = false;
    this.setConnection(false);
  }

  /** @param {boolean} poor The server's connection verdict for this page. */
  setConnection(poor) {
    this.setClient({ connection: poor ? 'poor' : 'ok' });
  }

  /** @param {StreamInfo|null} info The server's `stream_info`. */
  setInfo(info) {
    window.stream_info = info || null;
    this._changed();
  }

  /** @param {Partial<StreamClient>} description What the core learned of its own side. */
  setClient(description) {
    const client = this._client;
    if (Object.keys(description).every((key) => client[key] === description[key])) return;
    Object.assign(client, description);
    this._changed();
  }

  /**
   * The server's `stream_stats`, which the next sample carries. A sample on
   * screen that has none of the server's figures yet, the first after an
   * opening, takes them at once, so the first reading waits for neither side
   * and every later one costs a dashboard a single read.
   * @param {StreamSample} stats
   */
  serverSample(stats) {
    this._server = stats || {};
    this._serverAt = performance.now();
    const latest = window.stream_stats.latest;
    if (this._open && latest && !latest.server) {
      Object.assign(latest, this._server, { server: true });
      this._changed();
    }
  }

  /** @param {number} bytes Stream bytes that arrived, for the bandwidth and received figures. */
  noteBytes(bytes) {
    this._bytes += bytes;
    this._received += bytes;
  }

  /**
   * One second's figures from the core, merged with the server's and appended
   * to the history. `mbps` comes from `noteBytes` unless the core measured it,
   * and `received_mb` is what `noteBytes` counted since the opening.
   * @param {StreamSample} figures
   */
  clientSample(figures) {
    if (!this._open) return;
    const now = performance.now();
    const elapsed = (now - this._bytesAt) / 1000;
    const fresh = now - this._serverAt <= SERVER_FRESH_MS;
    const sample = Object.assign({ t: Date.now(), server: fresh }, fresh ? this._server : {}, figures);
    if (sample.mbps === undefined && elapsed > 0) {
      sample.mbps = Math.round((this._bytes * 8 / 1e6 / elapsed) * 100) / 100;
    }
    sample.received_mb = Math.round(this._received / 1e4) / 100;
    this._bytes = 0;
    this._bytesAt = now;
    const stats = window.stream_stats;
    stats.latest = sample;
    stats.history.push(sample);
    if (stats.history.length > HISTORY_MAX) stats.history.shift();
    this._changed();
  }
}
