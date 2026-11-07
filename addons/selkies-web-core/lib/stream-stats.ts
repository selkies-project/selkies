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
 * and the reason a faster path was declined (`ScreenCapture::stream_info`).
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

/** The server's description of the stream. */
export interface StreamInfo {
  backend: string;
  capture: string;
  zero_copy: boolean;
  zero_copy_available: boolean;
  capture_reason: string;
  encoder: string;
  hardware: boolean;
  gpu_present: boolean;
  hardware_expected: boolean;
  encoder_reason: string;
  codec: string;
  fullcolor: boolean;
  bit_depth?: number;
  striped: boolean;
  gpu: string;
  driver: string;
  encode_node: string;
  renderer?: string;
  render_node?: string;
  render_gpu?: string;
  renderer_reason?: string;
}

/** This page's description of the stream. */
export interface StreamClient {
  transport: 'websockets' | 'webrtc';
  decoder: 'hardware' | 'software' | 'unknown';
  decoder_evidence: string;
  hardware_expected: boolean;
  decoder_reason: string;
  codec: string;
  resolution: string;
  path: string;
  sink: string;
  decode_path: string;
  connection: 'ok' | 'poor';
}

/** One second's figures. */
export type StreamSample = Record<string, number | boolean | string> & { server?: boolean; t?: number };

/** A decoder verdict. */
export type DecoderVerdict = Pick<StreamClient, 'decoder' | 'decoder_evidence' | 'hardware_expected' | 'decoder_reason'>;

/**
 * What a decoded frame's pixel format says of the decoder behind it. A hardware
 * decoder hands out NV12 or an opaque frame, a software one planar I4xx.
 * @param format `VideoFrame.format`; undefined where no frame was seen.
 * @returns
 */
export function decoderOfFormat(format: string | null | undefined): 'hardware' | 'software' | 'unknown' {
  if (format === undefined) return 'unknown';
  if (format === null || format === 'NV12') return 'hardware';
  return /^I4/.test(format) ? 'software' : 'unknown';
}

/** What a decoded frame's pixel format is called in the evidence. */
const framesOf = (format: string | null | undefined): string => `${format === null ? 'Opaque' : format} frames`;

/**
 * A decoder verdict and what it rests on, with whether it fell short: software
 * where a hardware decoder was there to take the stream.
 * @param decoder
 * @param evidence
 * @param expected Whether a hardware decoder was there.
 * @param shortfall Why software fell short; said only where it did.
 * @returns
 */
function verdict(decoder: 'hardware' | 'software' | 'unknown', evidence: string, expected: boolean, shortfall = 'hardware_available'): DecoderVerdict {
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
 * @param capable
 * @param evidence What little there is otherwise.
 * @returns
 */
function capabilityVerdict(capable: boolean | null | undefined, evidence: string): DecoderVerdict {
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
  key = '';
  efficient: boolean | null = null;

  /**
   * @param configuration What `decodingInfo` is asked.
   * @param key The configuration the answer is kept for.
   */
  ask(configuration: MediaDecodingConfiguration, key: string): void {
    if (key === this.key) return;
    this.key = key;
    this.efficient = null;
    if (!navigator.mediaCapabilities || !navigator.mediaCapabilities.decodingInfo) return;
    navigator.mediaCapabilities.decodingInfo(configuration).then((info: MediaCapabilitiesDecodingInfo) => {
      if (this.key === key) this.efficient = info.supported ? !!info.powerEfficient : null;
    }).catch(() => {});
  }
}

/** Evidence for the WebCodecs verdict. */
export interface WebcodecsEvidence {
  forcedSoftware: boolean;
  hardwareSupported: boolean | null | undefined;
  format: string | null | undefined;
  capable: boolean | null | undefined;
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
 * @param evidence `hardwareSupported` is `VideoDecoder.isConfigSupported` with
 *     `prefer-hardware`, null where unasked; `capable` is `DecodeCapability.efficient`.
 * @returns
 */
export function webcodecsDecoder({ forcedSoftware, hardwareSupported, format, capable }: WebcodecsEvidence): DecoderVerdict {
  if (forcedSoftware) return verdict('software', '', true, 'software_preferred');
  if (hardwareSupported === false) return verdict('software', 'No hardware decoder for this stream', false);
  const seen = decoderOfFormat(format);
  if (seen !== 'unknown') return verdict(seen, framesOf(format), capable === true);
  return capabilityVerdict(capable, format === undefined ? '' : framesOf(format));
}

/** Evidence for the WebRTC verdict. */
export interface WebrtcEvidence {
  implementation: string | undefined;
  powerEfficient: boolean | undefined;
  trackFormat: string | null | undefined;
  elementFormat: string | null | undefined;
  capable: boolean | null | undefined;
}

/**
 * The WebRTC decoder verdict. Engines name the decoder in the inbound video
 * report only to a page that holds a capture permission. Without one, a frame
 * read from the received track is the decoder's own, as the WebCodecs verdict
 * reads it; the element's current frame is not, since an engine may copy a
 * software picture into GPU memory to composite it, so only a planar one says
 * anything (software). Last is whether the engine has an efficient decoder for
 * the stream at all, taken as the one in use where nothing said software.
 * @param evidence `trackFormat` is a
 *     `MediaStreamTrackProcessor` frame's format and `elementFormat` a
 *     `VideoFrame` of the `<video>`'s, undefined where none was read;
 *     `capable` is `DecodeCapability.efficient`.
 * @returns
 */
export function webrtcDecoder({ implementation, powerEfficient, trackFormat, elementFormat, capable }: WebrtcEvidence): DecoderVerdict {
  const named = implementation && implementation !== 'unknown' ? implementation : '';
  const expected = capable === true;
  if (typeof powerEfficient === 'boolean') return verdict(powerEfficient ? 'hardware' : 'software', named, expected);
  if (named) return verdict(/libvpx|ffmpeg|dav1d|openh264|libaom/i.test(named) ? 'software' : 'hardware', named, expected);
  const fromTrack = decoderOfFormat(trackFormat);
  if (fromTrack !== 'unknown') return verdict(fromTrack, framesOf(trackFormat), expected);
  if (decoderOfFormat(elementFormat) === 'software') return verdict('software', framesOf(elementFormat), expected);
  return capabilityVerdict(capable, '');
}

/** Options for `StreamStats`. */
export interface StreamStatsOptions {
  transport: 'websockets' | 'webrtc';
  send: (message: string) => void;
  isViewer: () => boolean;
  onOpenChange?: (open: boolean) => void;
}

/** The stream stats published on `window`. */
export interface StreamStatsWindow {
  open: boolean;
  latest: StreamSample | null;
  history: StreamSample[];
}

export class StreamStats {
  _send: (message: string) => void;
  _isViewer: () => boolean;
  _onOpenChange: (open: boolean) => void;
  _open = false;
  _subscribed = false;
  _server: StreamSample = {};
  _serverAt = -Infinity;
  _bytes = 0;
  _bytesAt = performance.now();
  _received = 0;
  _announcing = false;
  _client: StreamClient;

  /**
   * @param options `send` puts one text message on the session connection; `onOpenChange`
   *     lets the core start and stop its own sampling.
   */
  constructor({ transport, send, isViewer, onOpenChange }: StreamStatsOptions) {
    this._send = send;
    this._isViewer = isViewer;
    this._onOpenChange = onOpenChange || (() => {});
    this._client = { transport, decoder: 'unknown', decoder_evidence: '', hardware_expected: false,
      decoder_reason: '', codec: '', resolution: '', path: '', sink: '', decode_path: '', connection: 'ok' };
    (window as unknown as { stream_info: StreamInfo | null }).stream_info = null;
    (window as unknown as { stream_client: StreamClient }).stream_client = this._client;
    (window as unknown as { stream_stats: StreamStatsWindow }).stream_stats = { open: false, latest: null, history: [] };
  }

  /** Whether a dashboard has its stats on screen. */
  get open(): boolean {
    return this._open;
  }

  /** Announces a change, once for everything that changed in the same task. */
  _changed(): void {
    if (this._announcing || typeof window.dispatchEvent !== 'function') return;
    this._announcing = true;
    queueMicrotask(() => {
      this._announcing = false;
      window.dispatchEvent(new Event(STATS_EVENT));
    });
  }

  /**
   * A dashboard opened or shut its stats. Opening starts a fresh history.
   * @param open
   */
  setOpen(open: boolean): void {
    open = !!open;
    if (open === this._open) return;
    this._open = open;
    this._bytes = 0;
    this._bytesAt = performance.now();
    this._received = 0;
    (window as unknown as { stream_stats: StreamStatsWindow }).stream_stats = { open, latest: null, history: [] };
    this.subscribe();
    this._onOpenChange(open);
    this._changed();
  }

  /** Tells the server what this page wants; a fresh connection has to be told again. */
  subscribe(): void {
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
  disconnected(): void {
    this._subscribed = false;
    this.setConnection(false);
  }

  /** @param poor The server's connection verdict for this page. */
  setConnection(poor: boolean): void {
    this.setClient({ connection: poor ? 'poor' : 'ok' });
  }

  /** @param info The server's `stream_info`. */
  setInfo(info: StreamInfo | null): void {
    (window as unknown as { stream_info: StreamInfo | null }).stream_info = info || null;
    this._changed();
  }

  /** @param description What the core learned of its own side. */
  setClient(description: Partial<StreamClient>): void {
    const client = this._client;
    if (Object.keys(description).every((key) => (client as unknown as Record<string, unknown>)[key] === (description as unknown as Record<string, unknown>)[key])) return;
    Object.assign(client, description);
    this._changed();
  }

  /**
   * The server's `stream_stats`, which the next sample carries. A sample on
   * screen that has none of the server's figures yet, the first after an
   * opening, takes them at once, so the first reading waits for neither side
   * and every later one costs a dashboard a single read.
   * @param stats
   */
  serverSample(stats: StreamSample): void {
    this._server = stats || {};
    this._serverAt = performance.now();
    const latest = (window as unknown as { stream_stats: StreamStatsWindow }).stream_stats.latest;
    if (this._open && latest && !latest.server) {
      Object.assign(latest, this._server, { server: true });
      this._changed();
    }
  }

  /** @param bytes Stream bytes that arrived, for the bandwidth and received figures. */
  noteBytes(bytes: number): void {
    this._bytes += bytes;
    this._received += bytes;
  }

  /**
   * One second's figures from the core, merged with the server's and appended
   * to the history. `mbps` comes from `noteBytes` unless the core measured it,
   * and `received_mb` is what `noteBytes` counted since the opening.
   * @param figures
   */
  clientSample(figures: StreamSample): void {
    if (!this._open) return;
    const now = performance.now();
    const elapsed = (now - this._bytesAt) / 1000;
    const fresh = now - this._serverAt <= SERVER_FRESH_MS;
    const sample = Object.assign({ t: Date.now(), server: fresh }, fresh ? this._server : {}, figures) as StreamSample;
    if (sample.mbps === undefined && elapsed > 0) {
      sample.mbps = Math.round((this._bytes * 8 / 1e6 / elapsed) * 100) / 100;
    }
    sample.received_mb = Math.round(this._received / 1e4) / 100;
    this._bytes = 0;
    this._bytesAt = now;
    const stats = (window as unknown as { stream_stats: StreamStatsWindow }).stream_stats;
    stats.latest = sample;
    stats.history.push(sample);
    if (stats.history.length > HISTORY_MAX) stats.history.shift();
    this._changed();
  }
}
