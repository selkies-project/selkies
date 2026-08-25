/**
 * Webcam capture for the WebSocket transport.
 *
 * getUserMedia video frames are encoded in the page with WebCodecs (the codec
 * the encode worker measures as keeping up, H.264 or VP8) and handed to a
 * transport-supplied sender one encoded frame at a time; the server's virtual
 * camera decodes them. Engines without WebCodecs fall back to JPEG frames
 * drawn from a canvas, which the server passes on as an MJPEG device or
 * decodes just the same. The WebRTC transport does not use this
 * class: it attaches the camera track to a sendonly transceiver
 * (lib/webrtc.js `setWebcam`) and the browser's own encoder takes over.
 *
 * Frame orientation: the upright transform is relayed with every encoded
 * frame instead of being drawn into the pixels. It is read from VideoFrame
 * rotation/flip where the engine exposes them, and derived from the window
 * orientation where it does not (Safari, whose camera frames keep the
 * sensor's fixed orientation). VideoEncoder rejects a mid-stream orientation
 * change, so the encoder is rebuilt whenever the value moves. The JPEG rung
 * relays nothing: drawImage bakes the orientation the engine knows.
 *
 * Frames are read off the track through the first of these that the engine
 * offers: MediaStreamTrackProcessor on the page (Chromium), the standard
 * worker-only MediaStreamTrackProcessor with the track transferred into a
 * DedicatedWorker (Safari 18+), and a `<video>` element sampled with
 * requestVideoFrameCallback into VideoFrames (Firefox and anything else). The
 * last rung encodes like the others: it delivers the camera's own rate, and
 * the worker drops an input frame rather than queue it when the encoder is
 * behind, so no encoder drifts behind real time.
 *
 * Encoding runs off the page thread when the engine allows: the encode
 * worker is opened first, and an engine that transfers the camera track into
 * it (Safari, and Firefox as it ships transferable tracks) reads and encodes
 * there so frames never touch the page; otherwise the page reads frames its
 * own way and transfers each to the worker, and without a worker at all it
 * feeds a page-thread encoder through the same source paths.
 * @module
 */

/** Codec id of independent JPEG frames, as the server's webcam module numbers them. */
export const WEBCAM_CODEC_MJPEG = 0;
/** Codec id of H.264 Annex B frames. */
export const WEBCAM_CODEC_H264 = 1;
/** Codec id of VP8 frames. */
export const WEBCAM_CODEC_VP8 = 2;
/** Codec id of VP9 frames; never produced here, reserved on the wire. */
export const WEBCAM_CODEC_VP9 = 3;

/**
 * Encoder candidates in preference order. Reporting support is no promise of
 * speed, so the worker probe encodes through each and takes the first that
 * keeps up with the capture rate (Firefox's software H.264 tops out well
 * under 30 fps at 720p, where its VP8 runs three times faster); the rest are
 * tried on failure.
 */
const ENCODER_CANDIDATES = [
  { id: WEBCAM_CODEC_H264, name: "h264", codec: "avc1.42E01F", extra: { avc: { format: "annexb" } } },
  { id: WEBCAM_CODEC_VP8, name: "vp8", codec: "vp8", extra: {} },
];

/**
 * A keyframe at least this often bounds recovery after a lost frame even
 * when no request arrives from the server.
 */
const KEYFRAME_INTERVAL_MS = 4000;

/**
 * Frames are admitted by a credit that fills at the configured rate and costs
 * one frame interval to spend, capped at this many intervals. A camera
 * delivering at that rate then passes whole however its delivery jitters (and
 * it always does, down to the compositor quantizing a `<video>` element's
 * frame callbacks), while a faster source is still thinned to the rate asked
 * for. Comparing each gap against the interval instead drops every jittered
 * frame and halves the uplink.
 */
const FRAME_CREDIT_INTERVALS = 2;

/**
 * Closes a VideoFrame; the `<video>` element the no-WebCodecs path hands
 * over has nothing to close.
 * @param {VideoFrame|HTMLVideoElement} frame
 */
const closeFrame = (frame) => {
  if (frame && typeof frame.close === "function") frame.close();
};
/** Frames the worker source may have in flight to the page before it drops. */
const WORKER_MAX_IN_FLIGHT = 2;

/** Whether VideoFrames carry their upright transform as readable metadata. */
const HAS_FRAME_ORIENTATION =
  typeof VideoFrame !== "undefined" && typeof VideoFrame.prototype === "object" &&
  "rotation" in VideoFrame.prototype;

/**
 * The window orientation at which the camera sensor delivers upright frames on
 * the engines that hand its own orientation over.
 */
const SENSOR_UPRIGHT_ORIENTATION = -90;

/** The transform of a frame that is already upright. */
const UPRIGHT = { rotation: 0, flip: false };

/**
 * Clockwise rotation that makes a sensor-orientation frame upright, from the
 * current window orientation.
 * @returns {number} Degrees, a multiple of 90.
 */
const deriveRotation = () =>
  ((window.orientation - SENSOR_UPRIGHT_ORIENTATION) % 360 + 360) % 360;

/**
 * Whether this page has to derive what the frames do not carry. Only mobile
 * WebKit needs it: the one engine whose MediaStreamTrackProcessor lives in a
 * worker alone, and whose camera frames keep the sensor's fixed orientation
 * with no metadata to read it from. A worker source proves the engine and this
 * proves the viewport, because a desktop window has no orientation to derive
 * from and every other engine either pre-rotates the camera or exposes the
 * transform.
 * @returns {boolean}
 */
const canDeriveOrientation = () =>
  !HAS_FRAME_ORIENTATION && typeof window.orientation === "number";

/**
 * Source of the frame-reader worker for engines whose
 * MediaStreamTrackProcessor exists only in workers: the transferred track is
 * read there and each VideoFrame transferred to the page, at most
 * `WORKER_MAX_IN_FLIGHT` unacknowledged at a time.
 */
const MEDIA_WORKER_SRC = `
let reader = null, track = null, inFlight = 0;
self.onmessage = async (e) => {
  const m = e.data;
  if (m.type === 'ack') { if (inFlight > 0) inFlight--; return; }
  if (m.type === 'stop') { try { reader && reader.cancel(); } catch (err) {} try { track && track.stop(); } catch (err) {} reader = null; return; }
  if (m.type !== 'source') return;
  track = m.track;
  try { reader = new MediaStreamTrackProcessor({ track }).readable.getReader(); }
  catch (err) { self.postMessage({ type: 'failed' }); return; }
  self.postMessage({ type: 'ready' });
  for (;;) {
    let r;
    try { r = await reader.read(); } catch (err) { break; }
    if (r.done) break;
    if (inFlight >= ${WORKER_MAX_IN_FLIGHT}) { r.value.close(); continue; }
    inFlight++;
    self.postMessage({ type: 'frame', frame: r.value }, [r.value]);
  }
  self.postMessage({ type: 'end' });
};
`;

/**
 * Source of the worker that runs the WebCodecs encoder off the page thread.
 * The page hands it VideoFrames (transferable, unlike a whole track), or an
 * engine that allows it transfers the camera track itself, and the worker
 * posts back only the encoded chunks for the page to send. The heavy encode
 * never touches the UI thread, and a backgrounded tab throttles the page's
 * event loop but not a worker's, so the encoder keeps pace instead of
 * falling behind and breaking its reference chain into a permanent desync.
 * `probe` measures the candidates here before the page commits; an engine
 * without WebCodecs in workers reports `unsupported` and the page encodes on
 * its own thread instead.
 */
const ENCODE_WORKER_SRC = `
const CANDIDATES = ${JSON.stringify(ENCODER_CANDIDATES)};
const KEYFRAME_INTERVAL_MS = ${KEYFRAME_INTERVAL_MS};
const HAS_FRAME_ORIENTATION = ${HAS_FRAME_ORIENTATION};
// Frames one candidate is measured with, and the time that measurement may
// take: enough to leave the first keyframe behind, little enough that a slow
// encoder is ranked from what it did finish rather than delaying the camera.
const PROBE_FRAMES = 8;
const PROBE_BUDGET_MS = 400;
let encoder = null, candIndex = 0, cand = null, configuring = false, active = true;
let fps = 30, bitrate = 2500000, encodedSize = null, forceKeyframe = true, lastKeyframeMs = 0;
let trackReader = null, trackRef = null;
// The upright transform every chunk is stamped with, and the one the encoder
// latched: they differ where the page derives what the frames do not carry, and
// then no rebuild is owed for a turn the encoder never sees.
let orientation = { rotation: 0, flip: false }, derived = null;

function encoderConfig(c, w, h) {
  return { codec: c.codec, width: w, height: h, bitrate, framerate: fps, latencyMode: 'realtime', ...c.extra };
}

async function makeEncoder(w, h, latched) {
  configuring = true;
  while (candIndex < CANDIDATES.length) {
    cand = CANDIDATES[candIndex];
    let support = null;
    try { support = await VideoEncoder.isConfigSupported(encoderConfig(cand, w, h)); } catch (err) { support = null; }
    if (!active) { configuring = false; return false; }
    if (!support || !support.supported) { candIndex++; continue; }
    try {
      const enc = new VideoEncoder({
        output: (chunk) => {
          if (!active) return;
          const buf = new ArrayBuffer(chunk.byteLength);
          chunk.copyTo(new Uint8Array(buf));
          self.postMessage({ type: 'chunk', codecId: cand.id, keyframe: chunk.type === 'key', buffer: buf,
                             rotation: orientation.rotation, flip: orientation.flip }, [buf]);
        },
        error: (err) => { try { encoder && encoder.close(); } catch (x) {} encoder = null; encodedSize = null; candIndex++; },
      });
      enc.configure(support.config || encoderConfig(cand, w, h));
      encoder = enc; encodedSize = { w: w, h: h, rotation: latched.rotation, flip: latched.flip };
      forceKeyframe = true; configuring = false;
      self.postMessage({ type: 'ready', codec: cand.name });
      return true;
    } catch (err) { candIndex++; }
  }
  encoder = null; encodedSize = null; configuring = false;
  self.postMessage({ type: 'unsupported' });
  return false;
}

function encodeWith(frame, w, h) {
  if (!encoder || encoder.state !== 'configured' || !active) { frame.close(); return; }
  // The encoder is behind: drop this input frame (never an encoded output) so no
  // reference chain breaks; a worker rarely reaches this, which is the point.
  if (encoder.encodeQueueSize > 1) { frame.close(); return; }
  const now = performance.now();
  const keyFrame = forceKeyframe || now - lastKeyframeMs >= KEYFRAME_INTERVAL_MS;
  try {
    encoder.encode(frame, { keyFrame: keyFrame });
    if (keyFrame) { lastKeyframeMs = now; forceKeyframe = false; }
  } catch (err) { try { encoder.close(); } catch (x) {} encoder = null; encodedSize = null; candIndex++; }
  frame.close();
}

function orientationOf(frame) {
  return HAS_FRAME_ORIENTATION
    ? { rotation: frame.rotation || 0, flip: !!frame.flip }
    : { rotation: 0, flip: false };
}

function handleFrame(frame, label) {
  if (!active) { frame.close(); return; }
  const w = frame.displayWidth || frame.codedWidth;
  const h = frame.displayHeight || frame.codedHeight;
  const latched = orientationOf(frame);
  orientation = label || (HAS_FRAME_ORIENTATION ? latched : (derived || orientation));
  if (configuring) { frame.close(); return; }
  if (!encoder || !encodedSize || encodedSize.w !== w || encodedSize.h !== h ||
      encodedSize.rotation !== latched.rotation || encodedSize.flip !== latched.flip) {
    makeEncoder(w, h, latched).then(() => { if (encoder) encodeWith(frame, w, h); else frame.close(); });
    return;
  }
  encodeWith(frame, w, h);
}

// Frames per second one candidate sustains at w x h, or 0 if it cannot encode.
async function measure(c, w, h) {
  let support = null;
  try { support = await VideoEncoder.isConfigSupported(encoderConfig(c, w, h)); } catch (err) { return 0; }
  if (!support || !support.supported) return 0;
  const canvas = new OffscreenCanvas(w, h);
  const ctx = canvas.getContext('2d', { alpha: false });
  let encoded = 0, failed = false;
  const enc = new VideoEncoder({ output: () => { encoded++; }, error: () => { failed = true; } });
  try { enc.configure(support.config || encoderConfig(c, w, h)); } catch (err) { return 0; }
  const started = performance.now();
  for (let i = 0; i <= PROBE_FRAMES && !failed; i++) {
    ctx.fillStyle = 'hsl(' + (i * 40) + ',70%,50%)';
    ctx.fillRect(0, 0, w, h);
    ctx.fillStyle = '#fff';
    ctx.fillRect((i * 37) % w, (i * 23) % h, w >> 3, h >> 3);
    let frame = null;
    try {
      frame = new VideoFrame(canvas, { timestamp: Math.round(i * 1e6 / fps) });
      enc.encode(frame, { keyFrame: i === 0 });
    } catch (err) { failed = true; }
    if (frame) frame.close();
  }
  await Promise.race([
    enc.flush().catch(() => { failed = true; }),
    new Promise((r) => setTimeout(r, PROBE_BUDGET_MS)),
  ]);
  const rate = encoded / ((performance.now() - started) / 1000);
  try { enc.close(); } catch (err) { /* already closed */ }
  return failed ? 0 : rate;
}

self.onmessage = async (e) => {
  const m = e.data;
  if (m.type === 'frame') { handleFrame(m.frame, { rotation: m.rotation, flip: m.flip }); return; }
  // The page derives what a track this worker reads cannot tell it: no window here.
  if (m.type === 'orientation') { derived = { rotation: m.rotation, flip: m.flip }; return; }
  if (m.type === 'track') {
    // Combined read+encode: read the transferred camera track in this worker, so
    // frames never reach the page thread. Needs a worker MediaStreamTrackProcessor.
    if (typeof MediaStreamTrackProcessor === 'undefined') { self.postMessage({ type: 'track_unsupported' }); return; }
    try { trackReader = new MediaStreamTrackProcessor({ track: m.track }).readable.getReader(); }
    catch (err) { self.postMessage({ type: 'track_unsupported' }); return; }
    trackRef = m.track;
    self.postMessage({ type: 'track_reading' });
    (async () => {
      for (;;) {
        let r;
        try { r = await trackReader.read(); } catch (err) { break; }
        if (r.done || !active) { if (r.value) r.value.close(); break; }
        handleFrame(r.value);
      }
    })();
    return;
  }
  if (m.type === 'keyframe') { forceKeyframe = true; return; }
  if (m.type === 'config') { if (m.fps) fps = m.fps; if (m.bitrate) bitrate = m.bitrate; return; }
  if (m.type === 'stop') {
    active = false;
    try { trackReader && trackReader.cancel(); } catch (x) {}
    try { trackRef && trackRef.stop(); } catch (x) {}
    trackReader = null; trackRef = null;
    try { encoder && encoder.close(); } catch (x) {} encoder = null;
    return;
  }
  if (m.type === 'probe') {
    fps = m.fps || 30; bitrate = m.bitrate || 2500000;
    if (typeof VideoEncoder === 'undefined') { self.postMessage({ type: 'unsupported' }); return; }
    const w = m.width || 1280, h = m.height || 720;
    let best = -1, bestRate = 0;
    for (let i = 0; i < CANDIDATES.length; i++) {
      const rate = await measure(CANDIDATES[i], w, h);
      if (rate > bestRate) { best = i; bestRate = rate; }
      if (rate >= fps) break;
    }
    if (best < 0) { self.postMessage({ type: 'unsupported' }); return; }
    candIndex = best;
    self.postMessage({ type: 'probed', codec: CANDIDATES[best].name, rate: Math.round(bestRate) });
  }
};
`;

/**
 * @typedef {Object} WebcamCaptureOptions
 * @property {(codecId: number, keyframe: boolean, bytes: Uint8Array, rotation?: number, flip?: boolean) => void} sendFrame
 *     Delivers one encoded frame. `rotation` (clockwise degrees) and `flip`
 *     (horizontal, applied after the rotation) make its pixels upright and
 *     are 0 and false when they already are.
 * @property {(active: boolean) => void} [onStateChange] Called when capture starts and stops.
 * @property {(error: Error) => void} [onError] Called with getUserMedia and encoder failures.
 * @property {() => boolean} [canSend] Returning false skips a frame (backpressure).
 * @property {number} [width] Capture width hint, 1280 by default.
 * @property {number} [height] Capture height hint, 720 by default.
 * @property {number} [fps] Frame rate hint and send cadence cap, 30 by default.
 * @property {number} [bitrate] Encoder bitrate in bits per second, 2500000 by default.
 * @property {number} [quality] JPEG quality on the fallback rung, 0.8 by default.
 */

/**
 * Camera uplink for the WebSocket transport: opens the camera, settles on a
 * frame source and an encoder, and hands encoded frames to `sendFrame`.
 */
export class WebcamCapture {
  /** @param {WebcamCaptureOptions} opts */
  constructor(opts) {
    this._sendFrame = opts.sendFrame;
    this._onStateChange = opts.onStateChange || (() => {});
    this._onError = opts.onError || (() => {});
    this._canSend = opts.canSend || (() => true);
    this.width = opts.width || 1280;
    this.height = opts.height || 720;
    this.fps = opts.fps || 30;
    this.bitrate = opts.bitrate || 2500000;
    this.quality = opts.quality || 0.8;

    this._stream = null;
    this._track = null;
    this._source = null;
    this._encoder = null;
    this._encoderCodec = null;
    this._candidateIndex = 0;
    this._encodedSize = null;
    this._forceKeyframe = true;
    this._chainBroken = false;
    this._lastKeyframeMs = 0;
    this._lastFrameMs = 0;
    this._frameCredit = 0;
    this._canvas = null;
    this._ctx = null;
    this._jpegBusy = false;
    this._configuring = false;
    this._deriveOrientation = false;
    this._orientation = UPRIGHT;
    this._orientationWatch = null;
    this._active = false;
    this._generation = 0;
    this._encodeWorker = null;
    this._workerIsSource = false;
    this._encoderCodecName = null;
  }

  /** Whether a capture is running. @type {boolean} */
  get active() {
    return this._active;
  }

  /** Name of the codec frames are sent as (`h264`, `vp8`, `mjpeg`), or null. @type {?string} */
  get codec() {
    if (this._encoderCodecName) return this._encoderCodecName;
    return this._encoderCodec ? this._encoderCodec.name : (this._active ? "mjpeg" : null);
  }

  /**
   * Logs which source and encoder the session settled on, once per message:
   * the difference between explaining a client's CPU cost and guessing at it.
   * @param {string} message
   */
  _logPath(message) {
    if (!this._loggedPaths) {
      this._loggedPaths = new Set();
    }
    if (this._loggedPaths.has(message)) {
      return;
    }
    this._loggedPaths.add(message);
    console.info(`[Webcam] ${message}`);
  }

  /**
   * Opens the camera and starts sending. Failures are reported through
   * `onError` rather than thrown, and a track that ends (device unplugged,
   * permission revoked) stops the capture.
   * @param {string=} deviceId Camera to open; the default device otherwise.
   */
  async start(deviceId) {
    if (this._active) {
      return;
    }
    if (!navigator.mediaDevices || !navigator.mediaDevices.getUserMedia) {
      this._onError(new Error("getUserMedia unavailable"));
      return;
    }
    const video = {
      width: { ideal: this.width },
      height: { ideal: this.height },
      frameRate: { ideal: this.fps },
    };
    if (deviceId) {
      video.deviceId = { exact: deviceId };
    }
    let stream;
    try {
      stream = await navigator.mediaDevices.getUserMedia({ video, audio: false });
    } catch (error) {
      this._onError(error);
      return;
    }
    const track = stream.getVideoTracks()[0];
    if (!track) {
      stream.getTracks().forEach((t) => t.stop());
      this._onError(new Error("no video track"));
      return;
    }
    this._stream = stream;
    this._track = track;
    this._active = true;
    this._forceKeyframe = true;
    this._chainBroken = false;
    this._lastFrameMs = 0;
    this._frameCredit = 0;
    const generation = ++this._generation;
    track.addEventListener("ended", () => {
      if (this._generation === generation) {
        this.stop();
      }
    });
    this._onStateChange(true);
    this._source = await this._openCapture(track, generation);
    if (this._generation !== generation) {
      if (this._source) {
        this._source.close();
      }
      return;
    }
    if (!this._source) {
      this._onError(new Error("no frame source for the camera track"));
      this.stop();
    }
  }

  /** Makes the next frame a keyframe: the server lost its decoder reference or just started. */
  requestKeyframe() {
    this._forceKeyframe = true;
    if (this._encodeWorker) {
      try {
        this._encodeWorker.postMessage({ type: "keyframe" });
      } catch (e) {
        /* ignore */
      }
    }
  }

  /** Stops the capture and releases the camera, encoder and workers; idempotent. */
  stop() {
    if (!this._active && !this._stream) {
      return;
    }
    this._generation++;
    this._active = false;
    this._stopEncodeWorker();
    if (this._source) {
      this._source.close();
      this._source = null;
    }
    if (this._encoder) {
      try {
        this._encoder.close();
      } catch (e) {
        /* already closed */
      }
      this._encoder = null;
      this._encoderCodec = null;
    }
    this._configuring = false;
    this._deriveOrientation = false;
    this._orientation = UPRIGHT;
    this._unwatchOrientation();
    if (this._stream) {
      this._stream.getTracks().forEach((t) => {
        try {
          t.stop();
        } catch (e) {
          /* ignore */
        }
      });
      this._stream = null;
      this._track = null;
    }
    this._canvas = null;
    this._ctx = null;
    this._encodedSize = null;
    this._candidateIndex = 0;
    this._encoderCodecName = null;
    this._chainBroken = false;
    this._lastFrameMs = 0;
    this._frameCredit = 0;
    this._onStateChange(false);
  }

  /**
   * Opens the encode worker, then the frame source: the combined
   * read-and-encode worker when the engine transfers the track, else a page
   * source that feeds the worker or the page-thread encoder.
   * @param {MediaStreamTrack} track
   * @param {number} generation Capture generation; a later one cancels this.
   * @returns {Promise<?{close: function(): void}>} Source handle, or null with no source.
   */
  async _openCapture(track, generation) {
    await this._openEncodeWorker(generation);
    if (this._generation !== generation) return null;
    if (this._encodeWorker) {
      const combined = await this._tryCombinedWorker(track, generation);
      if (this._generation !== generation) {
        if (combined) combined.close();
        return null;
      }
      if (combined) return combined;
    }
    return this._openSource(track, generation);
  }

  /**
   * Tries to hand the whole camera track to the encode worker for combined
   * read-and-encode. Resolves to a source handle when the worker takes it
   * (the engine transfers tracks and the worker has a
   * MediaStreamTrackProcessor), else null so the page reads frames and feeds
   * the worker one at a time. A clone is transferred, so a refusal
   * (DataCloneError on Chromium) leaves the original track intact for that
   * fallback.
   * @param {MediaStreamTrack} track
   * @param {number} generation
   * @returns {Promise<?{close: function(): void}>}
   */
  _tryCombinedWorker(track, generation) {
    const worker = this._encodeWorker;
    if (!worker) return Promise.resolve(null);
    let clone;
    try {
      clone = track.clone();
    } catch (error) {
      return Promise.resolve(null);
    }
    return new Promise((resolve) => {
      let settled = false;
      const finish = (value) => {
        if (settled) return;
        settled = true;
        worker.removeEventListener("message", onMessage);
        clearTimeout(timer);
        resolve(value);
      };
      const onMessage = (e) => {
        const m = e.data;
        if (m.type === "track_reading") {
          this._workerIsSource = true;
          this._deriveOrientation = canDeriveOrientation();
          this._watchOrientation();
          this._logPath("capture+encode: camera read and encoded in a worker");
          finish({ close: () => this._stopEncodeWorker() });
        } else if (m.type === "track_unsupported") {
          try { clone.stop(); } catch (error) { /* ignore */ }
          finish(null);
        }
      };
      const timer = setTimeout(() => {
        try { clone.stop(); } catch (error) { /* ignore */ }
        finish(null);
      }, 2000);
      worker.addEventListener("message", onMessage);
      try {
        worker.postMessage({ type: "track", track: clone }, [clone]);
      } catch (error) {
        try { clone.stop(); } catch (e) { /* ignore */ }
        finish(null);
      }
    });
  }

  /**
   * Stands up the worker that encodes the VideoFrames the page hands it
   * (transferred, zero-copy) and posts back encoded chunks. Resolves once
   * `probe` confirms a codec encodes in the worker (`_handleFrame` then
   * routes frames to it), or leaves `_encodeWorker` null so the page encodes
   * on its own thread. A worker that reports `unsupported` or an error
   * before the page commits is dropped for page encoding, and one that does
   * so mid-stream (a codec dropped out) is dropped the same way, the next
   * frame re-routing to the page.
   * @param {number} generation
   * @returns {Promise<void>}
   */
  _openEncodeWorker(generation) {
    if (typeof VideoEncoder === "undefined" || typeof Worker === "undefined") {
      return Promise.resolve();
    }
    let worker;
    try {
      const url = URL.createObjectURL(new Blob([ENCODE_WORKER_SRC], { type: "text/javascript" }));
      worker = new Worker(url);
      URL.revokeObjectURL(url);
    } catch (error) {
      return Promise.resolve();
    }
    return new Promise((resolve) => {
      let settled = false;
      const done = () => { if (!settled) { settled = true; resolve(); } };
      const drop = (why) => {
        console.warn("[Webcam] encode-worker unavailable, encoding on the page:", why);
        const wasSource = this._workerIsSource;
        if (this._encodeWorker === worker) this._encodeWorker = null;
        this._workerIsSource = false;
        try { worker.terminate(); } catch (e) { /* ignore */ }
        done();
        // A worker that was reading the camera itself takes the capture with it:
        // re-open the page's own source so the frames keep coming.
        if (wasSource && this._active && this._generation === generation) {
          this._source = null;
          this._openSource(this._track, generation).then((source) => {
            if (this._generation !== generation) {
              if (source) source.close();
              return;
            }
            this._source = source;
            if (!source) this._onError(new Error("no frame source for the camera track"));
          });
        }
      };
      const timer = setTimeout(() => drop("probe timeout"), 3000);
      worker.onmessage = (e) => {
        const m = e.data;
        if (m.type === "probed") {
          clearTimeout(timer);
          this._encodeWorker = worker;
          this._encoderCodecName = m.codec;
          this._logPath(`encode: ${m.codec} in a worker, ${m.rate} fps measured against ${this.fps} asked for`);
          done();
          return;
        }
        if (m.type === "ready") { this._encoderCodecName = m.codec; return; }
        if (m.type === "chunk") {
          if (this._active && this._generation === generation) {
            this._deliverEncoded(m.codecId, m.keyframe, new Uint8Array(m.buffer), m.rotation, m.flip);
          }
          return;
        }
        if (m.type === "unsupported" || m.type === "error") {
          clearTimeout(timer);
          drop(m.type);
        }
      };
      worker.onerror = (ev) => { clearTimeout(timer); drop("worker.onerror: " + (ev && ev.message)); };
      this._encodeWorker = worker;
      try {
        worker.postMessage({ type: "probe", width: this.width, height: this.height, fps: this.fps, bitrate: this.bitrate });
      } catch (error) {
        clearTimeout(timer);
        drop("probe postMessage threw: " + error);
      }
    });
  }

  /**
   * Watches the window orientation for a track the encode worker reads itself:
   * that worker is out of reach of the window the transform is derived from, so
   * the page pushes it in on every turn.
   */
  _watchOrientation() {
    if (!this._deriveOrientation || this._orientationWatch) return;
    const push = () => this._pushOrientation();
    this._orientationWatch = push;
    window.addEventListener("orientationchange", push);
    if (screen.orientation && screen.orientation.addEventListener) {
      screen.orientation.addEventListener("change", push);
    }
    push();
  }

  /** Drops the orientation listeners; idempotent. */
  _unwatchOrientation() {
    const push = this._orientationWatch;
    this._orientationWatch = null;
    if (!push) return;
    window.removeEventListener("orientationchange", push);
    if (screen.orientation && screen.orientation.removeEventListener) {
      screen.orientation.removeEventListener("change", push);
    }
  }

  /** Sends the window's current upright transform to the encode worker. */
  _pushOrientation() {
    if (!this._encodeWorker) return;
    this._orientation = { rotation: deriveRotation(), flip: false };
    try {
      this._encodeWorker.postMessage({ type: "orientation", ...this._orientation });
    } catch (e) {
      /* ignore */
    }
  }

  /** Tears down the encode worker (idempotent); the page-thread encoder takes over. */
  _stopEncodeWorker() {
    const worker = this._encodeWorker;
    this._encodeWorker = null;
    this._workerIsSource = false;
    if (worker) {
      try { worker.postMessage({ type: "stop" }); } catch (e) { /* ignore */ }
      setTimeout(() => { try { worker.terminate(); } catch (e) { /* ignore */ } }, 100);
    }
  }

  /**
   * Opens the first page frame source the engine offers, in the order the
   * module docblock gives. The worker-only MediaStreamTrackProcessor means
   * Safari, whose raw sensor frames carry no readable orientation, so their
   * rotation is derived per frame. The `<video>` element pins the JPEG rung:
   * the encode worker is dropped and every encoder candidate skipped.
   * @param {MediaStreamTrack} track
   * @param {number} generation
   * @returns {Promise<?{close: function(): void}>}
   */
  async _openSource(track, generation) {
    if (typeof MediaStreamTrackProcessor !== "undefined") {
      try {
        const processor = new MediaStreamTrackProcessor({ track });
        this._logPath("capture: MediaStreamTrackProcessor on the page");
        return this._readerSource(processor.readable.getReader(), generation);
      } catch (error) {
        /* fall through to the other sources */
      }
    }
    const worker = await this._workerSource(track, generation);
    if (worker) {
      this._deriveOrientation = canDeriveOrientation();
      this._logPath("capture: MediaStreamTrackProcessor in a worker");
      return worker;
    }
    this._logPath("capture: <video> element sampled with requestVideoFrameCallback");
    return this._videoSource(track, generation);
  }

  /**
   * MediaStreamTrackProcessor on the page: drains at camera cadence, never
   * queues.
   * @param {ReadableStreamDefaultReader<VideoFrame>} reader
   * @param {number} generation
   * @returns {{close: function(): void}}
   */
  _readerSource(reader, generation) {
    const loop = async () => {
      for (;;) {
        let result;
        try {
          result = await reader.read();
        } catch (error) {
          break;
        }
        if (result.done || this._generation !== generation) {
          if (result.value) result.value.close();
          break;
        }
        this._handleFrame(result.value);
      }
    };
    loop();
    return {
      close: () => {
        try {
          reader.cancel();
        } catch (e) {
          /* ignore */
        }
      },
    };
  }

  /**
   * Standard mediacapture-transform: the processor only exists in workers,
   * so a clone of the track is transferred in and VideoFrames are
   * transferred back. Resolves null when the worker cannot read the track.
   * @param {MediaStreamTrack} track
   * @param {number} generation
   * @returns {Promise<?{close: function(): void}>}
   */
  _workerSource(track, generation) {
    return new Promise((resolve) => {
      let clone;
      try {
        clone = track.clone();
      } catch (error) {
        resolve(null);
        return;
      }
      let worker;
      try {
        const url = URL.createObjectURL(new Blob([MEDIA_WORKER_SRC], { type: "text/javascript" }));
        worker = new Worker(url);
        URL.revokeObjectURL(url);
      } catch (error) {
        try { clone.stop(); } catch (e) { /* ignore */ }
        resolve(null);
        return;
      }
      let settled = false;
      const finish = (value) => {
        if (!settled) {
          settled = true;
          resolve(value);
        }
      };
      const timer = setTimeout(() => {
        worker.terminate();
        try { clone.stop(); } catch (e) { /* ignore */ }
        finish(null);
      }, 3000);
      const source = {
        close: () => {
          try { worker.postMessage({ type: "stop" }); } catch (e) { /* ignore */ }
          setTimeout(() => worker.terminate(), 100);
        },
      };
      worker.onerror = () => {
        clearTimeout(timer);
        worker.terminate();
        try { clone.stop(); } catch (e) { /* ignore */ }
        finish(null);
      };
      worker.onmessage = (e) => {
        const m = e.data;
        if (!m) return;
        if (m.type === "ready") {
          clearTimeout(timer);
          finish(source);
        } else if (m.type === "failed") {
          clearTimeout(timer);
          worker.terminate();
          try { clone.stop(); } catch (err) { /* ignore */ }
          finish(null);
        } else if (m.type === "frame") {
          worker.postMessage({ type: "ack" });
          if (this._generation !== generation) {
            m.frame.close();
            return;
          }
          this._handleFrame(m.frame);
        } else if (m.type === "end") {
          worker.terminate();
        }
      };
      try {
        worker.postMessage({ type: "source", track: clone }, [clone]);
      } catch (error) {
        clearTimeout(timer);
        worker.terminate();
        try { clone.stop(); } catch (e) { /* ignore */ }
        finish(null);
      }
    });
  }

  /**
   * A `<video>` element sampled with requestVideoFrameCallback (or a timer
   * without it); the element must stay in the DOM, visually inert, or engines
   * stop decoding for it. Each sample becomes a VideoFrame the encoder takes,
   * through a canvas on an engine that refuses the element as a frame source;
   * with no WebCodecs at all the element itself is handed to the JPEG rung.
   * @param {MediaStreamTrack} track
   * @param {number} generation
   * @returns {{close: function(): void}}
   */
  _videoSource(track, generation) {
    const video = document.createElement("video");
    video.muted = true;
    video.playsInline = true;
    video.autoplay = true;
    video.style.cssText = "position:fixed;top:0;left:0;width:1px;height:1px;opacity:0;pointer-events:none;";
    document.body.appendChild(video);
    video.srcObject = new MediaStream([track]);
    let handle = null;
    let timer = null;
    let canvas = null;
    let ctx = null;
    const sample = () => {
      if (this._generation !== generation) {
        return;
      }
      if (video.readyState >= 2 && video.videoWidth > 0 && typeof VideoFrame === "undefined") {
        // No WebCodecs at all: the JPEG rung draws the element itself.
        this._handleFrame(video);
      } else if (video.readyState >= 2 && video.videoWidth > 0) {
        let frame = null;
        const timestamp = Math.round(performance.now() * 1000);
        try {
          frame = new VideoFrame(video, { timestamp });
        } catch (error) {
          // Engines that reject <video> as a VideoFrame source go through a canvas.
          if (!canvas) {
            canvas = document.createElement("canvas");
            ctx = canvas.getContext("2d", { alpha: false, desynchronized: true });
          }
          if (canvas.width !== video.videoWidth || canvas.height !== video.videoHeight) {
            canvas.width = video.videoWidth;
            canvas.height = video.videoHeight;
          }
          try {
            ctx.drawImage(video, 0, 0);
            frame = new VideoFrame(canvas, { timestamp });
          } catch (e) {
            frame = null;
          }
        }
        if (frame) {
          this._handleFrame(frame);
        }
      }
      if (video.requestVideoFrameCallback) {
        handle = video.requestVideoFrameCallback(sample);
      }
    };
    const playback = video.play();
    if (playback && playback.catch) playback.catch(() => {});
    if (video.requestVideoFrameCallback) {
      handle = video.requestVideoFrameCallback(sample);
    } else {
      timer = setInterval(sample, 1000 / this.fps);
    }
    return {
      close: () => {
        if (handle !== null && video.cancelVideoFrameCallback) {
          try { video.cancelVideoFrameCallback(handle); } catch (e) { /* ignore */ }
        }
        if (timer !== null) clearInterval(timer);
        try { video.srcObject = null; } catch (e) { /* ignore */ }
        video.remove();
      },
    };
  }

  /**
   * Receives one frame from whichever source is open: a VideoFrame this
   * method now owns, or the `<video>` element on the pinned JPEG rung. A
   * frame is always closed. Frames arriving faster than `fps` or while
   * `canSend` refuses are dropped. With an encode worker the frame is
   * transferred to it, zero-copy; the worker only takes VideoFrames, so a
   * `<video>`-element frame or a dead worker throws and encoding drops back
   * to this thread from then on. On the page-thread encoder a changed size
   * or orientation rebuilds the encoder, since a VideoEncoder latches the
   * orientation of its first frame and rejects any other; one rebuild runs
   * at a time and frames racing it are dropped, never queued.
   * @param {VideoFrame|HTMLVideoElement} frame
   */
  _handleFrame(frame) {
    if (!this._active || !this._canSend()) {
      closeFrame(frame);
      return;
    }
    const now = performance.now();
    if (!this._admit(now)) {
      closeFrame(frame);
      return;
    }
    if (this._encodeWorker) {
      try {
        const label = this._frameOrientation(frame);
        this._encodeWorker.postMessage({ type: "frame", frame, ...label }, [frame]);
        return;
      } catch (error) {
        this._stopEncodeWorker();
        closeFrame(frame);
        return;
      }
    }
    if (typeof VideoEncoder === "undefined" || this._candidateIndex >= ENCODER_CANDIDATES.length) {
      this._encodeJpeg(frame, now);
      return;
    }
    const w = frame.displayWidth || frame.codedWidth;
    const h = frame.displayHeight || frame.codedHeight;
    const latched = { rotation: frame.rotation || 0, flip: !!frame.flip };
    this._orientation = this._frameOrientation(frame);
    const s = this._encodedSize;
    if (!this._encoder || !s || s.w !== w || s.h !== h ||
        s.rotation !== latched.rotation || s.flip !== latched.flip) {
      if (this._configuring) {
        frame.close();
        return;
      }
      this._configuring = true;
      this._configureEncoder(w, h, latched.rotation, latched.flip)
        .then(() => this._encodeWith(frame, w, h, now))
        .finally(() => {
          this._configuring = false;
        });
      return;
    }
    this._encodeWith(frame, w, h, now);
  }

  /**
   * Whether one frame arriving now fits the configured rate.
   * @param {number} now
   * @returns {boolean}
   */
  _admit(now) {
    const interval = 1000 / this.fps;
    const elapsed = this._lastFrameMs ? now - this._lastFrameMs : interval;
    this._lastFrameMs = now;
    this._frameCredit = Math.min(this._frameCredit + elapsed, interval * FRAME_CREDIT_INTERVALS);
    if (this._frameCredit < interval) {
      return false;
    }
    this._frameCredit -= interval;
    return true;
  }

  /**
   * The upright transform of one frame: what the engine put on it, or what the
   * window says where the engine puts nothing.
   * @param {VideoFrame|HTMLVideoElement} frame
   * @returns {{rotation: number, flip: boolean}}
   */
  _frameOrientation(frame) {
    if (this._deriveOrientation) {
      return { rotation: deriveRotation(), flip: false };
    }
    return { rotation: frame.rotation || 0, flip: !!frame.flip };
  }

  /**
   * Encodes one frame on the page-thread encoder. An encoder that is behind
   * (more than one frame queued) drops the frame rather than queueing
   * latency; one that throws moves on to the next candidate.
   * @param {VideoFrame} frame
   * @param {number} w
   * @param {number} h
   * @param {number} now `performance.now()` at receipt.
   */
  _encodeWith(frame, w, h, now) {
    const encoder = this._encoder;
    if (!encoder || encoder.state !== "configured" || !this._active) {
      frame.close();
      return;
    }
    if (encoder.encodeQueueSize > 1) {
      frame.close();
      return;
    }
    const keyFrame = this._forceKeyframe || now - this._lastKeyframeMs >= KEYFRAME_INTERVAL_MS;
    try {
      encoder.encode(frame, { keyFrame });
      if (keyFrame) {
        this._lastKeyframeMs = now;
        this._forceKeyframe = false;
      }
    } catch (error) {
      this._onEncoderFailure(error);
    }
    frame.close();
  }

  /**
   * Builds the encoder for the first candidate the engine supports at the
   * given size, stamping the orientation its frames carry onto every chunk it
   * emits. Resolves when an encoder is configured or every candidate was
   * rejected, after which frames take the JPEG path.
   * @param {number} w
   * @param {number} h
   * @param {number} rotation Clockwise degrees.
   * @param {boolean} flip Horizontal flip after the rotation.
   * @returns {Promise<void>}
   */
  async _configureEncoder(w, h, rotation = 0, flip = false) {
    const generation = this._generation;
    if (this._encoder) {
      try { this._encoder.close(); } catch (e) { /* ignore */ }
      this._encoder = null;
      this._encoderCodec = null;
    }
    while (this._candidateIndex < ENCODER_CANDIDATES.length) {
      const cand = ENCODER_CANDIDATES[this._candidateIndex];
      const config = {
        codec: cand.codec,
        width: w,
        height: h,
        bitrate: this.bitrate,
        framerate: this.fps,
        latencyMode: "realtime",
        ...cand.extra,
      };
      let support = null;
      try {
        support = await VideoEncoder.isConfigSupported(config);
      } catch (error) {
        support = null;
      }
      if (this._generation !== generation) {
        return;
      }
      if (!support || !support.supported) {
        this._candidateIndex++;
        continue;
      }
      try {
        const encoder = new VideoEncoder({
          output: (chunk) => this._onChunk(cand, chunk, generation),
          error: (error) => this._onEncoderFailure(error),
        });
        encoder.configure(support.config || config);
        this._encoder = encoder;
        this._encoderCodec = cand;
        this._encodedSize = { w, h, rotation, flip };
        this._forceKeyframe = true;
        this._logPath(`encoder: ${cand.name} (${cand.codec}) at ${w}x${h}`);
        return;
      } catch (error) {
        this._candidateIndex++;
      }
    }
    this._encodedSize = null;
  }

  /**
   * Sends one encoded frame to the transport. A frame dropped because the
   * socket is backed up breaks the chain the server's decoder follows, so
   * nothing but a keyframe is sent until one is asked for, and it is only
   * asked for once the socket can take it: a keyframe encoded into a full
   * socket is dropped like any other frame. Independent JPEG frames carry no
   * such dependency and do not come through here.
   * @param {number} codecId
   * @param {boolean} keyframe
   * @param {Uint8Array} bytes
   * @param {number} [rotation] Clockwise degrees that make the frame upright.
   * @param {boolean} [flip] Horizontal mirror, applied after the rotation.
   */
  _deliverEncoded(codecId, keyframe, bytes, rotation, flip) {
    if (!this._canSend()) {
      this._chainBroken = true;
      return;
    }
    if (this._chainBroken && !keyframe) {
      this.requestKeyframe();
      return;
    }
    this._sendFrame(codecId, keyframe, bytes, rotation, flip);
    if (keyframe) {
      this._chainBroken = false;
    }
  }

  /**
   * Output of the page-thread encoder: copies the chunk out and sends it with
   * the transform of the frame it came from.
   * @param {{id: number}} cand Encoder candidate that produced the chunk.
   * @param {EncodedVideoChunk} chunk
   * @param {number} generation
   */
  _onChunk(cand, chunk, generation) {
    if (this._generation !== generation || !this._active) {
      return;
    }
    const buf = new Uint8Array(chunk.byteLength);
    chunk.copyTo(buf);
    this._deliverEncoded(cand.id, chunk.type === "key", buf,
                         this._orientation.rotation, this._orientation.flip);
  }

  /**
   * Abandons an encoder that failed after reporting support (Firefox does
   * this for H.264) for the next candidate; the JPEG path is the final
   * fallback.
   * @param {Error} error
   */
  _onEncoderFailure(error) {
    console.warn("Webcam encoder failed, trying the next codec:", error);
    if (this._encoder) {
      try { this._encoder.close(); } catch (e) { /* ignore */ }
      this._encoder = null;
      this._encoderCodec = null;
    }
    this._candidateIndex++;
    this._encodedSize = null;
  }

  /**
   * Encodes a frame as JPEG through `OffscreenCanvas.convertToBlob`, one
   * encode in flight at a time. A JPEG frame always leaves upright and carries
   * no transform on the wire: drawImage bakes in the one the engine put on the
   * frame (whose display size already counts the turn), and a derived one is
   * applied here as a canvas transform.
   * @param {VideoFrame|HTMLVideoElement} frame
   * @param {number} now `performance.now()` at receipt.
   */
  _encodeJpeg(frame, now) {
    if (this._jpegBusy) {
      closeFrame(frame);
      return;
    }
    const turn = this._deriveOrientation ? deriveRotation() : 0;
    const sideways = turn % 180 === 90;
    const dw = frame.displayWidth || frame.codedWidth || frame.videoWidth;
    const dh = frame.displayHeight || frame.codedHeight || frame.videoHeight;
    const w = sideways ? dh : dw;
    const h = sideways ? dw : dh;
    if (!this._canvas) {
      this._logPath("encoder: JPEG through OffscreenCanvas");
      this._canvas = new OffscreenCanvas(w, h);
      this._ctx = this._canvas.getContext("2d", { alpha: false, desynchronized: true });
    } else if (this._canvas.width !== w || this._canvas.height !== h) {
      this._canvas.width = w;
      this._canvas.height = h;
    }
    try {
      if (turn) {
        this._ctx.save();
        this._ctx.translate(w / 2, h / 2);
        this._ctx.rotate((turn * Math.PI) / 180);
        this._ctx.drawImage(frame, -dw / 2, -dh / 2, dw, dh);
        this._ctx.restore();
      } else {
        this._ctx.drawImage(frame, 0, 0, w, h);
      }
    } catch (error) {
      closeFrame(frame);
      return;
    }
    closeFrame(frame);
    this._jpegBusy = true;
    const generation = this._generation;
    this._canvas
      .convertToBlob({ type: "image/jpeg", quality: this.quality })
      .then((blob) => blob.arrayBuffer())
      .then((buf) => {
        if (this._active && this._generation === generation) {
          this._sendFrame(WEBCAM_CODEC_MJPEG, true, new Uint8Array(buf));
        }
      })
      .catch((error) => this._onError(error))
      .finally(() => {
        this._jpegBusy = false;
      });
  }
}
