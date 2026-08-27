/**
 * Webcam capture for the WebSocket transport.
 *
 * getUserMedia frames are encoded in the page with WebCodecs (H.264, else
 * VP8) and handed to a transport-supplied sender one encoded frame at a
 * time; the server's virtual camera decodes them. Codecs earn their place
 * empirically: the probe ranks candidates on the camera's own frames and
 * rejects one whose output decodes to the wrong picture
 * (`PROBE_COLOUR_TOLERANCE`); after it, a frame offered to a busy encoder is
 * dropped rather than queued, a frame the encoder sits on counts the same
 * way (`createLagGauge`), and the share behind moves the uplink down the
 * ladder (`createEncodePace`). Past the last codec, and with no WebCodecs at
 * all, JPEG frames from a canvas: more bytes at the camera's own rate. The
 * WebRTC transport instead attaches the track to a sendonly transceiver
 * (lib/webrtc.js `setWebcam`).
 *
 * Orientation is relayed with each encoded frame, never drawn into the
 * pixels: read from VideoFrame rotation/flip where exposed, derived from the
 * window orientation where not (Safari's sensor-fixed frames). VideoEncoder
 * rejects a mid-stream orientation change, so a turn rebuilds the encoder;
 * the JPEG rung relays nothing, drawImage bakes what the engine knows.
 *
 * Frames come off the track through the first source the engine offers:
 * MediaStreamTrackProcessor on the page (Chromium), the worker-only
 * processor with the track transferred in (Safari 18+), or a `<video>`
 * element sampled with requestVideoFrameCallback (Firefox and the rest).
 * That last source feeds the ladder only when `webcam_encoder` names a
 * codec -- an engine landing there can hold the camera rate on a software
 * encoder at the cost of a whole core, which no probe can price -- so `auto`
 * sends its samples to the JPEG rung. Encoding runs in a worker when the
 * engine allows (a transferable track never touches the page; otherwise
 * each frame is transferred), else on the page thread through the same
 * sources.
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
 * Candidates in preference order; support reports are no promise of speed,
 * so the probe measures each on real frames (Firefox's software H.264 tops
 * out under 30 fps at 720p where its VP8 runs three times faster).
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
 * Frame admission credit: fills at the configured rate, one interval per
 * frame, capped here. Delivery always jitters (compositors quantize
 * `<video>` callbacks), so a per-gap limiter would drop every jittered frame
 * and halve the uplink; the credit passes a camera at rate whole.
 */
const FRAME_CREDIT_INTERVALS = 2;

/**
 * `webcam_encoder` values: `auto` = the ladder on MediaStreamTrackProcessor
 * sources and JPEG on the `<video>` rung, `h264`/`vp8` = that codec alone
 * everywhere (JPEG still the floor), `mjpeg` = JPEG everywhere.
 */
export const WEBCAM_ENCODER_PREFERENCES = ["auto", "h264", "vp8", "mjpeg"];

/** Frames the encode pace is measured over before it can be believed. */
export const PACE_MIN_SAMPLES = 60;
/** Share of offered frames the encoder may drop before it counts as too slow. */
export const PACE_BEHIND_RATIO = 1 / 6;
/**
 * Capture intervals the oldest unanswered frame may age before the encoder
 * counts as behind: some encoders hold frames in a pipeline `encodeQueueSize`
 * never shows (Firefox H.264 runs tens of seconds stale at a queue of two).
 * Half a second at 30 fps.
 */
export const PACE_LAG_INTERVALS = 15;
/**
 * Region-mean colour error between a probe frame and its own decoded output,
 * past which the candidate encodes the wrong picture (some Firefox GPU
 * stacks hand their encoder false chroma). Honest lossy encoding stays under
 * a third of this.
 */
export const PROBE_COLOUR_TOLERANCE = 48;

/**
 * Share of offered frames the encoder was behind for, measured on live
 * camera frames: the signal that a codec is too slow.
 * @returns {{note: function(boolean): void, tooSlow: function(): boolean,
 *   behindRatio: function(): number, reset: function(): void}}
 */
export function createEncodePace() {
  let offered = 0;
  let behind = 0;
  return {
    /** @param {boolean} wasBehind The frame was dropped for a busy encoder. */
    note(wasBehind) {
      offered++;
      if (wasBehind) behind++;
    },
    /** @returns {boolean} A whole window fell behind; true starts a fresh one. */
    tooSlow() {
      if (offered < PACE_MIN_SAMPLES) return false;
      const slow = behind / offered > PACE_BEHIND_RATIO;
      offered = 0;
      behind = 0;
      return slow;
    },
    /** @returns {number} Share of frames dropped so far in this window. */
    behindRatio() {
      return offered ? behind / offered : 0;
    },
    /** Starts a fresh window. */
    reset() {
      offered = 0;
      behind = 0;
    },
  };
}

/**
 * Staleness of the oldest frame sent to the encoder and not yet answered by
 * a chunk; answering settles everything up to its timestamp, so an encoder
 * that quietly discards inputs is not held to them. Stringified into the
 * encode worker: keep it self-contained apart from `PACE_LAG_INTERVALS`.
 * @param {number} fps Capture rate the budget is scaled by.
 * @returns {{budgetMs: number, sent: function(number, number): void,
 *   answered: function(number): void, lagMs: function(number): number,
 *   reset: function(): void}}
 */
export function createLagGauge(fps) {
  const budgetMs = (PACE_LAG_INTERVALS * 1000) / (fps > 0 ? fps : 30);
  let pending = [];
  return {
    budgetMs,
    /**
     * @param {number} timestamp Frame timestamp in microseconds.
     * @param {number} wall Wallclock milliseconds at hand-over.
     */
    sent(timestamp, wall) {
      pending.push({ timestamp, wall });
    },
    /** @param {number} timestamp Chunk timestamp settling every frame up to it. */
    answered(timestamp) {
      while (pending.length && pending[0].timestamp <= timestamp) pending.shift();
    },
    /**
     * @param {number} now Wallclock milliseconds.
     * @returns {number} Milliseconds the oldest unanswered frame has waited.
     */
    lagMs(now) {
      return pending.length ? now - pending[0].wall : 0;
    },
    /** Forgets every outstanding frame. */
    reset() {
      pending = [];
    },
  };
}

/**
 * Closes a VideoFrame; the `<video>` element the JPEG rung hands over has
 * nothing to close.
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
 * Only mobile WebKit derives orientation: the one engine with a worker-only
 * MediaStreamTrackProcessor and sensor-fixed frames carrying no transform.
 * The worker source proves the engine, `window.orientation` the viewport.
 * @returns {boolean}
 */
const canDeriveOrientation = () =>
  !HAS_FRAME_ORIENTATION && typeof window.orientation === "number";

/**
 * Frame-reader worker for engines whose MediaStreamTrackProcessor exists
 * only in workers: reads the transferred track, transfers each VideoFrame
 * back, at most `WORKER_MAX_IN_FLIGHT` unacknowledged.
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
 * Worker running the WebCodecs encoder off the page thread: the page hands
 * it VideoFrames (or transfers the whole track where allowed) and gets back
 * encoded chunks. A backgrounded tab throttles the page's loop but not a
 * worker's, so the encoder keeps pace instead of breaking its reference
 * chain into a desync. `probe` measures the candidates before the page
 * commits; no WebCodecs in workers reports `unsupported`.
 */
const ENCODE_WORKER_SRC = `
let CANDIDATES = ${JSON.stringify(ENCODER_CANDIDATES)};
const PACE_MIN_SAMPLES = ${PACE_MIN_SAMPLES};
const PACE_BEHIND_RATIO = ${PACE_BEHIND_RATIO};
const PACE_LAG_INTERVALS = ${PACE_LAG_INTERVALS};
const PROBE_COLOUR_TOLERANCE = ${PROBE_COLOUR_TOLERANCE};
const createLagGauge = ${createLagGauge.toString()};
const KEYFRAME_INTERVAL_MS = ${KEYFRAME_INTERVAL_MS};
const HAS_FRAME_ORIENTATION = ${HAS_FRAME_ORIENTATION};
// Frames taken from the camera to measure the candidates with, how long to wait
// for them, and the time one candidate may spend on them. Synthetic content
// cannot stand in: a codec costs what the lens shows, at the size it shows it.
const PROBE_SOURCE_FRAMES = 8;
const PROBE_WAIT_MS = 2500;
const PROBE_BUDGET_MS = 400;
// Share of the asked rate a candidate must clearly reach to start on: a few
// frames either way is noise, and the watchdog judges what the camera then
// does to it on real frames.
const PROBE_RATE_MARGIN = 0.9;
let encoder = null, candIndex = 0, cand = null, configuring = false, active = true;
// Camera frames held while probing; during the measurement itself further
// frames are dropped, so nothing reaches the wire unvetted.
let probing = false, measuring = false, probeFrames = [], probeTimer = null;
let fps = 30, bitrate = 2500000, encodedSize = null, forceKeyframe = true, lastKeyframeMs = 0;
let trackReader = null, trackRef = null;
// The pace window (see createEncodePace) and the staleness gauge that
// catches an encoder holding frames off-queue.
let paceOffered = 0, paceBehind = 0, lag = null;
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
          if (lag) lag.answered(chunk.timestamp);
          if (!active) return;
          const buf = new ArrayBuffer(chunk.byteLength);
          chunk.copyTo(new Uint8Array(buf));
          self.postMessage({ type: 'chunk', codecId: cand.id, keyframe: chunk.type === 'key', buffer: buf,
                             rotation: orientation.rotation, flip: orientation.flip }, [buf]);
        },
        error: (err) => { try { encoder && encoder.close(); } catch (x) {} encoder = null; encodedSize = null; lag = null; candIndex++; },
      });
      enc.configure(support.config || encoderConfig(cand, w, h));
      encoder = enc; encodedSize = { w: w, h: h, rotation: latched.rotation, flip: latched.flip };
      lag = createLagGauge(fps);
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
  const now = performance.now();
  // A visible queue and stale output both count as behind.
  const behind = encoder.encodeQueueSize > 1 || (lag !== null && lag.lagMs(now) > lag.budgetMs);
  paceOffered++;
  if (behind) paceBehind++;
  if (paceOffered >= PACE_MIN_SAMPLES) {
    const slow = paceBehind / paceOffered > PACE_BEHIND_RATIO;
    paceOffered = 0; paceBehind = 0;
    // Dropping or delaying this much of the camera is what a codec this
    // engine cannot encode in real time looks like: a core at full tilt and
    // a receiver falling behind. Take the next rung; the page takes the JPEG
    // one when this list runs out.
    if (slow) {
      self.postMessage({ type: 'slow', codec: cand ? cand.name : '' });
      try { if (encoder && encoder.state !== 'closed') encoder.close(); } catch (err) {}
      encoder = null; encodedSize = null; lag = null; candIndex++;
      if (candIndex >= CANDIDATES.length) self.postMessage({ type: 'exhausted' });
      frame.close();
      return;
    }
  }
  // The encoder is behind: drop this input frame (never an encoded output) so no
  // reference chain breaks; a worker rarely reaches this, which is the point.
  if (behind) { frame.close(); return; }
  const keyFrame = forceKeyframe || now - lastKeyframeMs >= KEYFRAME_INTERVAL_MS;
  try {
    encoder.encode(frame, { keyFrame: keyFrame });
    if (lag) lag.sent(frame.timestamp, now);
    if (keyFrame) { lastKeyframeMs = now; forceKeyframe = false; }
  } catch (err) { try { encoder.close(); } catch (x) {} encoder = null; encodedSize = null; lag = null; candIndex++; }
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
  if (probing) {
    if (measuring || probeFrames.length >= PROBE_SOURCE_FRAMES) { frame.close(); return; }
    // The window belongs to the camera once it is delivering. A device chosen
    // from several starts late, and a window spent waiting for it would expire
    // holding too few frames to measure anything.
    if (!probeFrames.length && probeTimer !== null) {
      clearTimeout(probeTimer);
      probeTimer = setTimeout(() => { if (probing) runProbe(w, h); }, PROBE_WAIT_MS);
    }
    probeFrames.push(frame);
    if (probeFrames.length >= PROBE_SOURCE_FRAMES) runProbe(w, h);
    return;
  }
  if (configuring) { frame.close(); return; }
  // A spent ladder was announced once; later frames are not a fresh verdict.
  if (!encoder && candIndex >= CANDIDATES.length) { frame.close(); return; }
  if (!encoder || !encodedSize || encodedSize.w !== w || encodedSize.h !== h ||
      encodedSize.rotation !== latched.rotation || encodedSize.flip !== latched.flip) {
    makeEncoder(w, h, latched).then(() => { if (encoder) encodeWith(frame, w, h); else frame.close(); });
    return;
  }
  encodeWith(frame, w, h);
}

// Frames per second one candidate sustains on the camera's own frames, or 0 if
// it cannot encode them.
async function measure(c, w, h, frames) {
  const out = { rate: 0, colErr: -1 };
  let support = null;
  try { support = await VideoEncoder.isConfigSupported(encoderConfig(c, w, h)); } catch (err) { return out; }
  if (!support || !support.supported) return out;
  let encoded = 0, failed = false;
  const chunks = [];
  const enc = new VideoEncoder({
    output: (chunk) => {
      encoded++;
      const data = new Uint8Array(chunk.byteLength);
      chunk.copyTo(data);
      chunks.push({ key: chunk.type === 'key', ts: chunk.timestamp, data });
    },
    error: () => { failed = true; },
  });
  try { enc.configure(support.config || encoderConfig(c, w, h)); } catch (err) { return out; }
  const started = performance.now();
  // The frames stay open: each candidate encodes the same ones, and the probe
  // closes them once the last candidate has had its turn.
  for (let i = 0; i < frames.length && !failed; i++) {
    try { enc.encode(frames[i], { keyFrame: i === 0 }); } catch (err) { failed = true; }
  }
  await Promise.race([
    enc.flush().catch(() => { failed = true; }),
    new Promise((r) => setTimeout(r, PROBE_BUDGET_MS)),
  ]);
  const rate = encoded / ((performance.now() - started) / 1000);
  try { enc.close(); } catch (err) { /* already closed */ }
  if (failed) return out;
  out.rate = rate;
  out.colErr = await colourError(frames[0], chunks, c);
  return out;
}

// Largest per-channel difference between region means of the first probe frame
// and its own decoded output, or -1 when there is nothing to judge with (an
// unjudged candidate passes). Drawn unscaled over a coarse grid: means converge
// however lossily grain encodes while false chroma moves whole regions, and
// drawImage downscaling point-samples on some engines.
async function colourError(frame, chunks, c) {
  if (typeof OffscreenCanvas === 'undefined' || typeof VideoDecoder === 'undefined' || !chunks.length) return -1;
  const cells = 4;
  const regionMeans = (source, w, h) => {
    const canvas = new OffscreenCanvas(w, h);
    const ctx = canvas.getContext('2d');
    ctx.drawImage(source, 0, 0);
    const d = ctx.getImageData(0, 0, w, h).data;
    const sums = new Float64Array(cells * cells * 3);
    const counts = new Float64Array(cells * cells);
    for (let y = 0; y < h; y++) {
      const rowCell = (((y * cells) / h) | 0) * cells;
      for (let x = 0; x < w; x++) {
        const cell = rowCell + (((x * cells) / w) | 0);
        const i = (y * w + x) * 4;
        sums[cell * 3] += d[i];
        sums[cell * 3 + 1] += d[i + 1];
        sums[cell * 3 + 2] += d[i + 2];
        counts[cell]++;
      }
    }
    for (let cell = 0; cell < counts.length; cell++) {
      sums[cell * 3] /= counts[cell];
      sums[cell * 3 + 1] /= counts[cell];
      sums[cell * 3 + 2] /= counts[cell];
    }
    return sums;
  };
  const decoded = [];
  try {
    const w = frame.displayWidth || frame.codedWidth;
    const h = frame.displayHeight || frame.codedHeight;
    const want = regionMeans(frame, w, h);
    let failed = false;
    const dec = new VideoDecoder({ output: (f) => decoded.push(f), error: () => { failed = true; } });
    dec.configure({ codec: c.codec });
    for (let i = 0; i < chunks.length; i++) {
      dec.decode(new EncodedVideoChunk({ type: chunks[i].key ? 'key' : 'delta', timestamp: chunks[i].ts, data: chunks[i].data }));
    }
    await dec.flush();
    let err = -1;
    if (decoded.length && !failed) {
      const got = regionMeans(decoded[decoded.length - 1], w, h);
      err = 0;
      for (let i = 0; i < want.length; i++) err = Math.max(err, Math.abs(want[i] - got[i]));
    }
    try { dec.close(); } catch (e2) { /* already closed */ }
    for (let i = 0; i < decoded.length; i++) { try { decoded[i].close(); } catch (e2) {} }
    return err;
  } catch (err) {
    for (let i = 0; i < decoded.length; i++) { try { decoded[i].close(); } catch (e2) {} }
    return -1;
  }
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
    if (Array.isArray(m.candidates) && m.candidates.length) CANDIDATES = m.candidates;
    if (typeof VideoEncoder === 'undefined') { self.postMessage({ type: 'unsupported' }); return; }
    probing = true; probeFrames = [];
    // A camera that never delivers must not hold the uplink: rank on whatever
    // arrived, and if nothing did, open on the first candidate and leave the
    // watchdog to answer for it.
    probeTimer = setTimeout(() => {
      if (probing) runProbe(m.width || 1280, m.height || 720);
    }, PROBE_WAIT_MS);
  }
};

// Ranks the candidates on the held camera frames and commits to one, or hands
// the page the JPEG rung when none of them keeps up with what the lens is
// showing at the size it is showing it.
async function runProbe(w, h) {
  if (!probing || measuring) return;
  measuring = true;
  if (probeTimer !== null) { clearTimeout(probeTimer); probeTimer = null; }
  const frames = probeFrames;
  probeFrames = [];
  // A partial set is no measurement: a few frames carry the keyframe and the
  // flush and read low enough to reject a codec that keeps up, so open on
  // the first candidate and let the watchdog answer for it.
  if (frames.length < PROBE_SOURCE_FRAMES) {
    probing = false; measuring = false;
    for (let i = 0; i < frames.length; i++) { try { frames[i].close(); } catch (err) {} }
    if (!active) return;
    self.postMessage({ type: 'probed', codec: CANDIDATES[0].name });
    return;
  }
  let best = -1, bestRate = 0, wrongColour = false;
  for (let i = 0; i < CANDIDATES.length; i++) {
    const m = await measure(CANDIDATES[i], w, h, frames);
    // Wrong colours are out however fast; the JPEG rung draws through the
    // reference's own path.
    if (m.colErr > PROBE_COLOUR_TOLERANCE) {
      wrongColour = true;
      self.postMessage({ type: 'wrongcolour', codec: CANDIDATES[i].name, colErr: Math.round(m.colErr) });
      continue;
    }
    if (m.rate > bestRate) { best = i; bestRate = m.rate; }
    if (m.rate >= fps) break;
  }
  probing = false; measuring = false;
  for (let i = 0; i < frames.length; i++) { try { frames[i].close(); } catch (err) {} }
  if (!active) return;
  if (best < 0) {
    candIndex = CANDIDATES.length;
    self.postMessage({ type: wrongColour ? 'exhausted' : 'unsupported' });
    return;
  }
  // Nothing near the asked rate: starting anyway spends a core to send a
  // fraction of the camera, which the JPEG rung exists to avoid. Merely-near
  // is taken; the watchdog answers for it.
  if (bestRate < fps * PROBE_RATE_MARGIN) {
    candIndex = CANDIDATES.length;
    self.postMessage({ type: 'exhausted', codec: CANDIDATES[best].name, rate: bestRate.toFixed(1) });
    return;
  }
  candIndex = best;
  self.postMessage({ type: 'probed', codec: CANDIDATES[best].name, rate: Math.round(bestRate) });
}
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
 * @property {string} [encoderPreference] A `WEBCAM_ENCODER_PREFERENCES` value
 *     (the `webcam_encoder` setting); `auto` by default.
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
    this.encoderPreference = WEBCAM_ENCODER_PREFERENCES.indexOf(opts.encoderPreference) >= 0
      ? opts.encoderPreference : "auto";
    this._encoderCandidates = this.encoderPreference === "h264" || this.encoderPreference === "vp8"
      ? ENCODER_CANDIDATES.filter((c) => c.name === this.encoderPreference)
      : ENCODER_CANDIDATES;

    this._stream = null;
    this._track = null;
    this._source = null;
    this._establishing = false;
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
    this._pace = createEncodePace();
    this._configuring = false;
    this._deriveOrientation = false;
    this._orientation = UPRIGHT;
    this._orientationWatch = null;
    this._active = false;
    this._generation = 0;
    this._encodeWorker = null;
    this._settleEncodeWorker = null;
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
   * source feeding the worker or the page-thread encoder. The probe ranks on
   * live frames, so a source must feed it while it decides; a combined
   * worker that dropped out took its track clone with it, so the page then
   * opens its own source.
   * @param {MediaStreamTrack} track
   * @param {number} generation Capture generation; a later one cancels this.
   * @returns {Promise<?{close: function(): void}>} Source handle, or null with no source.
   */
  async _openCapture(track, generation) {
    this._establishing = true;
    if (this.encoderPreference === "mjpeg") {
      this._candidateIndex = this._encoderCandidates.length;
      this._encoderCodecName = "mjpeg";
      this._logPath("encode: JPEG (webcam_encoder is mjpeg)");
    }
    const decided = this._openEncodeWorker(generation);
    try {
      let source = null;
      let combined = false;
      if (this._encodeWorker) {
        source = await this._tryCombinedWorker(track, generation);
        combined = !!source;
        if (this._generation !== generation) {
          if (source) source.close();
          return null;
        }
      }
      if (!source) source = await this._openSource(track, generation);
      if (this._generation !== generation) {
        if (source) source.close();
        return null;
      }
      await decided;
      if (this._generation !== generation) {
        if (source) source.close();
        return null;
      }
      if (combined && !this._encodeWorker) {
        source = await this._openSource(track, generation);
        if (this._generation !== generation) {
          if (source) source.close();
          return null;
        }
      }
      return source;
    } finally {
      this._establishing = false;
    }
  }

  /**
   * Hands the whole camera track to the encode worker for combined
   * read-and-encode; null when the engine cannot transfer tracks or the
   * worker lacks a MediaStreamTrackProcessor. A clone is transferred, so a
   * refusal (DataCloneError on Chromium) leaves the original for the
   * page-read fallback.
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
   * Stands up the encode worker and resolves once its probe commits a codec
   * (`_handleFrame` then routes frames to it), or leaves `_encodeWorker`
   * null for page-thread encoding. `unsupported` or an error drops it the
   * same way mid-stream, the next frame re-routing to the page; a worker
   * that was reading the camera takes the capture with it, so the page's own
   * source is re-opened.
   * @param {number} generation
   * @returns {Promise<void>}
   */
  _openEncodeWorker(generation) {
    if (typeof VideoEncoder === "undefined" || typeof Worker === "undefined" ||
        this._candidateIndex >= this._encoderCandidates.length) {
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
      const done = () => {
        if (settled) return;
        settled = true;
        if (this._settleEncodeWorker === settle) this._settleEncodeWorker = null;
        resolve();
      };
      // Lets _stopEncodeWorker resolve a probe still being awaited.
      const settle = () => { clearTimeout(timer); done(); };
      const drop = (why) => {
        console.warn("[Webcam] encode-worker unavailable, encoding on the page:", why);
        const wasSource = this._workerIsSource;
        if (this._encodeWorker === worker) this._encodeWorker = null;
        this._workerIsSource = false;
        try { worker.terminate(); } catch (e) { /* ignore */ }
        done();
        if (wasSource && !this._establishing && this._active && this._generation === generation) {
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
      const timer = setTimeout(() => drop("probe timeout"), 8000);
      worker.onmessage = (e) => {
        const m = e.data;
        if (m.type === "probed") {
          clearTimeout(timer);
          this._encodeWorker = worker;
          this._encoderCodecName = m.codec;
          this._logPath(m.rate !== undefined
            ? `encode: ${m.codec} in a worker, ${m.rate} fps measured against ${this.fps} asked for`
            : `encode: ${m.codec} in a worker, unmeasured (no camera frames to rank on)`);
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
        if (m.type === "slow") {
          this._logPath(`encode: ${m.codec} could not keep up with the camera; taking the next rung`);
          return;
        }
        if (m.type === "wrongcolour") {
          this._logPath(`encode: ${m.codec} decodes to the wrong colours on this engine (mean channel error ${m.colErr}); skipping it`);
          return;
        }
        if (m.type === "exhausted") {
          clearTimeout(timer);
          this._logPath(m.rate !== undefined
            ? `encode: no codec kept up with the camera (${m.codec} reached ${m.rate} fps against ${this.fps} asked for); encoding JPEG instead`
            : "encode: no codec kept up with the camera; encoding JPEG instead");
          this._stopEncodeWorker();
          this._candidateIndex = this._encoderCandidates.length;
          this._encoderCodecName = "mjpeg";
          done();
          return;
        }
        if (m.type === "unsupported" || m.type === "error") {
          clearTimeout(timer);
          drop(m.type);
        }
      };
      worker.onerror = (ev) => { clearTimeout(timer); drop("worker.onerror: " + (ev && ev.message)); };
      this._encodeWorker = worker;
      this._settleEncodeWorker = settle;
      try {
        worker.postMessage({ type: "probe", width: this.width, height: this.height, fps: this.fps,
                             bitrate: this.bitrate, candidates: this._encoderCandidates });
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

  /** Tears down the encode worker and settles a pending probe; idempotent. */
  _stopEncodeWorker() {
    const worker = this._encodeWorker;
    const settle = this._settleEncodeWorker;
    this._encodeWorker = null;
    this._settleEncodeWorker = null;
    this._workerIsSource = false;
    if (worker) {
      worker.onmessage = null;
      try { worker.postMessage({ type: "stop" }); } catch (e) { /* ignore */ }
      setTimeout(() => { try { worker.terminate(); } catch (e) { /* ignore */ } }, 100);
    }
    if (settle) settle();
  }

  /**
   * Opens the first page frame source the engine offers, in the module
   * docblock's order. A worker-only MediaStreamTrackProcessor means Safari,
   * whose sensor frames carry no readable orientation, so it is derived per
   * frame; the `<video>` element feeds the ladder only when `webcam_encoder`
   * names a codec and VideoFrames can be built from it, else the JPEG rung.
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
    if (typeof VideoFrame === "undefined") {
      this._pinJpegRung("no VideoFrame constructor");
    } else if (this.encoderPreference === "auto" || this.encoderPreference === "mjpeg") {
      this._pinJpegRung(`webcam_encoder is ${this.encoderPreference}`);
    } else {
      this._logPath("capture: <video> element sampled with requestVideoFrameCallback");
    }
    return this._videoSource(track, generation);
  }

  /**
   * Sends every later frame to the JPEG rung: the encode worker is dropped
   * and every encoder candidate skipped.
   * @param {string} why One clause for the path log.
   */
  _pinJpegRung(why) {
    this._stopEncodeWorker();
    this._candidateIndex = this._encoderCandidates.length;
    this._encoderCodecName = "mjpeg";
    this._logPath(`capture: <video> element sampled with requestVideoFrameCallback (JPEG: ${why})`);
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
   * Standard mediacapture-transform: the processor exists only in workers,
   * so a track clone is transferred in and VideoFrames transferred back;
   * null when the worker cannot read the track.
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
   * without it); the element must stay in the DOM, visually inert, or
   * engines stop decoding for it. While candidates remain each sample
   * becomes a VideoFrame (from the element, else through a canvas); past the
   * ladder, or after a second of failed frame building, the element itself
   * goes to the JPEG rung so a source that cannot build frames never starves
   * the uplink.
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
    let buildFailures = 0;
    const buildFrame = () => {
      const timestamp = Math.round(performance.now() * 1000);
      try {
        return new VideoFrame(video, { timestamp });
      } catch (error) {
        /* fall through to the canvas */
      }
      try {
        if (!canvas) {
          canvas = document.createElement("canvas");
          ctx = canvas.getContext("2d", { alpha: false, desynchronized: true });
        }
        if (canvas.width !== video.videoWidth || canvas.height !== video.videoHeight) {
          canvas.width = video.videoWidth;
          canvas.height = video.videoHeight;
        }
        ctx.drawImage(video, 0, 0);
        return new VideoFrame(canvas, { timestamp });
      } catch (error) {
        return null;
      }
    };
    const sample = () => {
      if (this._generation !== generation) {
        return;
      }
      if (video.readyState >= 2 && video.videoWidth > 0) {
        if (typeof VideoFrame !== "undefined" && this._candidateIndex < this._encoderCandidates.length) {
          const frame = buildFrame();
          if (frame) {
            buildFailures = 0;
            this._handleFrame(frame);
          } else if (++buildFailures > this.fps) {
            this._pinJpegRung("VideoFrames cannot be built from the element");
          }
        } else {
          this._handleFrame(video);
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
   * Receives one frame from whichever source is open (a VideoFrame this
   * method now owns, or the `<video>` element on the JPEG rung) and always
   * closes it; frames beyond `fps` or while `canSend` refuses are dropped.
   * With an encode worker the frame is transferred zero-copy, a dead worker
   * dropping encoding back to this thread; the page-thread encoder is
   * rebuilt on a changed size or orientation, one rebuild at a time, racing
   * frames dropped.
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
    if (typeof VideoEncoder === "undefined" || this._candidateIndex >= this._encoderCandidates.length) {
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
   * Encodes one frame on the page-thread encoder, dropped when more than one
   * is already queued; a throw moves to the next candidate.
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
    const behind = encoder.encodeQueueSize > 1;
    this._pace.note(behind);
    if (this._pace.tooSlow()) {
      this._onEncoderTooSlow();
      frame.close();
      return;
    }
    if (behind) {
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
   * Builds the encoder for the first supported candidate at the given size,
   * stamping the frames' orientation onto every chunk. Resolves once
   * configured or every candidate was rejected (frames then take JPEG).
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
    while (this._candidateIndex < this._encoderCandidates.length) {
      const cand = this._encoderCandidates[this._candidateIndex];
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
   * Sends one encoded frame. A frame dropped on a backed-up socket breaks
   * the chain the server's decoder follows, so nothing but a keyframe goes
   * out until one is asked for -- and only once the socket can take it.
   * Independent JPEG frames do not come through here.
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
   * Abandons a codec that cannot keep up for the next candidate, past the
   * last for the JPEG rung. Not a failure: it just cannot hold what the
   * camera is showing.
   */
  _onEncoderTooSlow() {
    const name = this._encoderCodec ? this._encoderCodec.name : "the encoder";
    if (this._encoder) {
      try { this._encoder.close(); } catch (e) { /* ignore */ }
      this._encoder = null;
      this._encoderCodec = null;
    }
    this._candidateIndex++;
    this._encodedSize = null;
    this._pace.reset();
    this._logPath(this._candidateIndex >= this._encoderCandidates.length
      ? `encode: ${name} could not keep up with the camera; encoding JPEG instead`
      : `encode: ${name} could not keep up with the camera; taking the next rung`);
  }

  /**
   * Abandons an encoder that failed after reporting support (Firefox H.264
   * does) for the next candidate; JPEG is the final fallback.
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
   * Encodes a frame as JPEG through `OffscreenCanvas.convertToBlob`, one in
   * flight at a time. A JPEG leaves upright with no transform on the wire:
   * drawImage bakes in the engine's, a derived one is applied as a canvas
   * transform.
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
