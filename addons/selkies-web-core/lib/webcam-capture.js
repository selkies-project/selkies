// Webcam capture for the WebSocket transport: getUserMedia video frames are
// encoded in the page with WebCodecs (H.264 first, VP8 second) and handed to a
// transport-supplied sender one encoded frame at a time; the server's virtual
// camera decodes them. Engines without WebCodecs, and engines whose only
// frame source is a <video> element (no MediaStreamTrackProcessor on the page
// or in a worker — Firefox), fall back to JPEG frames drawn from a canvas,
// which the server passes on as an MJPEG device or decodes just the same. The WebRTC
// transport does not use this class: it attaches the camera track to a
// sendonly transceiver (lib/webrtc.js setWebcam) and the browser's own encoder
// takes over.
//
// Frame orientation: the upright transform is relayed with every encoded
// frame instead of being drawn into the pixels. It is read from VideoFrame
// rotation/flip where the engine exposes them, and derived from the window
// orientation where it does not (Safari, whose camera frames keep the sensor's
// fixed orientation). VideoEncoder rejects a mid-stream orientation change, so
// the encoder is rebuilt whenever the value moves. The JPEG rung relays
// nothing: drawImage bakes the orientation the engine knows.
//
// Frames are read off the track through the first of these that the engine
// offers: MediaStreamTrackProcessor on the page (Chromium), the standard
// worker-only MediaStreamTrackProcessor with the track transferred into a
// DedicatedWorker (Safari 18+), and a <video> element sampled with
// requestVideoFrameCallback (Firefox and anything else). The last rung pins
// the JPEG path: with no track reader, every sample must be materialized on
// the page thread, and pushing those through a (software) VideoEncoder loads
// the page while the encoder drifts ever further behind real time.

export const WEBCAM_CODEC_MJPEG = 0;
export const WEBCAM_CODEC_H264 = 1;
export const WEBCAM_CODEC_VP8 = 2;
export const WEBCAM_CODEC_VP9 = 3;

// Encoder candidates in preference order; the first one the engine reports as
// supported (and that actually encodes) wins, the rest are tried on failure.
const ENCODER_CANDIDATES = [
  { id: WEBCAM_CODEC_H264, name: "h264", codec: "avc1.42E01F", extra: { avc: { format: "annexb" } } },
  { id: WEBCAM_CODEC_VP8, name: "vp8", codec: "vp8", extra: {} },
];

// A keyframe at least this often bounds recovery after a lost frame even when
// no request arrives from the server.
const KEYFRAME_INTERVAL_MS = 4000;

// A VideoFrame must be closed; the <video> element the no-WebCodecs path hands
// over has nothing to close.
const closeFrame = (frame) => {
  if (frame && typeof frame.close === "function") frame.close();
};
// Frames the worker source may have in flight to the page before it drops.
const WORKER_MAX_IN_FLIGHT = 2;

// Whether VideoFrames carry their upright transform as readable metadata.
const HAS_FRAME_ORIENTATION =
  typeof VideoFrame !== "undefined" && typeof VideoFrame.prototype === "object" &&
  "rotation" in VideoFrame.prototype;

// The window orientation at which the camera sensor delivers upright frames:
// -90 (landscape, camera edge at the bottom) on Apple tablets.
const SENSOR_UPRIGHT_ORIENTATION = -90;

// Clockwise rotation that makes a sensor-orientation frame upright, from the
// current window orientation (screen.orientation.angle where the legacy
// property is missing; 270 is that API's spelling of -90).
const deriveRotation = () => {
  let o = typeof window.orientation === "number"
    ? window.orientation
    : (screen.orientation && typeof screen.orientation.angle === "number" ? screen.orientation.angle : 0);
  if (o === 270) o = -90;
  return ((o - SENSOR_UPRIGHT_ORIENTATION) % 360 + 360) % 360;
};

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

// A worker that runs the WebCodecs encoder off the page thread. The page reads
// the camera on its own source (transferable VideoFrames, unlike a whole track)
// and hands each one here; the worker encodes and posts only the encoded chunk
// back for the page to send. The heavy encode never touches the UI thread, and a
// backgrounded tab throttles the page's event loop but not a worker's, so the
// encoder keeps pace instead of falling behind and breaking its reference chain
// (the permanent-desync failure). `probe` confirms a codec is encodable here
// before the page commits; anything without WebCodecs in workers reports
// 'unsupported' and the page encodes on its own thread instead.
const ENCODE_WORKER_SRC = `
const CANDIDATES = ${JSON.stringify(ENCODER_CANDIDATES)};
const KEYFRAME_INTERVAL_MS = ${KEYFRAME_INTERVAL_MS};
let encoder = null, candIndex = 0, cand = null, configuring = false, active = true;
let fps = 30, bitrate = 2500000, encodedSize = null, forceKeyframe = true, lastKeyframeMs = 0;
let trackReader = null, trackRef = null;

function encoderConfig(c, w, h) {
  return { codec: c.codec, width: w, height: h, bitrate, framerate: fps, latencyMode: 'realtime', ...c.extra };
}

async function makeEncoder(w, h) {
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
          self.postMessage({ type: 'chunk', codecId: cand.id, keyframe: chunk.type === 'key', buffer: buf }, [buf]);
        },
        error: (err) => { try { encoder && encoder.close(); } catch (x) {} encoder = null; encodedSize = null; candIndex++; },
      });
      enc.configure(support.config || encoderConfig(cand, w, h));
      encoder = enc; encodedSize = { w: w, h: h }; forceKeyframe = true; configuring = false;
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

function handleFrame(frame) {
  if (!active) { frame.close(); return; }
  const w = frame.displayWidth || frame.codedWidth;
  const h = frame.displayHeight || frame.codedHeight;
  if (configuring) { frame.close(); return; }
  if (!encoder || (encodedSize && (encodedSize.w !== w || encodedSize.h !== h))) {
    makeEncoder(w, h).then(() => { if (encoder) encodeWith(frame, w, h); else frame.close(); });
    return;
  }
  encodeWith(frame, w, h);
}

self.onmessage = async (e) => {
  const m = e.data;
  if (m.type === 'frame') { handleFrame(m.frame); return; }
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
    while (candIndex < CANDIDATES.length) {
      let sup = null;
      try { sup = await VideoEncoder.isConfigSupported(encoderConfig(CANDIDATES[candIndex], m.width || 1280, m.height || 720)); } catch (err) { sup = null; }
      if (sup && sup.supported) { self.postMessage({ type: 'probed' }); return; }
      candIndex++;
    }
    self.postMessage({ type: 'unsupported' });
  }
};
`;

export class WebcamCapture {
  // opts:
  //   sendFrame(codecId, keyframe, Uint8Array, rotation, flip)
  //     required, delivers one encoded frame; rotation (clockwise degrees) and
  //     flip (horizontal, after the rotation) make its pixels upright and are
  //     0/false when the pixels already are
  //   onStateChange(active)                     optional, called when capture starts/stops
  //   onError(error)                            optional, getUserMedia/encoder failures
  //   canSend()                                 optional, false skips a frame (backpressure)
  //   width, height, fps, bitrate, quality      capture hints; quality is the JPEG fallback quality
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
    this._lastSendMs = 0;
    this._canvas = null;
    this._ctx = null;
    this._jpegBusy = false;
    this._configuring = false;
    this._deriveOrientation = false;
    this._active = false;
    this._generation = 0;
    this._encodeWorker = null;
    this._encoderCodecName = null;
  }

  get active() {
    return this._active;
  }

  // Name of the codec frames are sent as ("h264", "vp8", "mjpeg") or null.
  get codec() {
    if (this._encoderCodecName) return this._encoderCodecName;
    return this._encoderCodec ? this._encoderCodec.name : (this._active ? "mjpeg" : null);
  }

  // Which source and encoder a session settled on, said once: the difference
  // between explaining a client's CPU cost and guessing at it.
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

  // The server lost its decoder reference (or just started): the next frame is a keyframe.
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
    this._onStateChange(false);
  }

  // --- capture ------------------------------------------------------------

  // Encode off the page thread when the engine can. The encode worker is opened
  // first; then, if the engine will transfer the camera track into it (Safari, and
  // Firefox as it ships transferable tracks), the worker reads and encodes the track
  // itself and frames never touch the page. Otherwise the page reads frames its own
  // way and hands each to the worker, and without a worker at all it feeds the
  // page-thread encoder — same source paths either way.
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

  // Try to hand the whole camera track to the encode worker for combined read+encode.
  // Resolves to a source handle when the worker takes it (engine transfers tracks and
  // the worker has a MediaStreamTrackProcessor), else null so the page reads frames
  // and feeds the worker one at a time. A clone is transferred, so a refusal
  // (DataCloneError on Chromium) leaves the original track intact for that fallback.
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

  // Stand up a worker that encodes the VideoFrames the page hands it (transferred,
  // zero-copy) and posts back encoded chunks. Resolves once `probe` confirms a
  // codec encodes in the worker (then `_handleFrame` routes frames to it), or
  // leaves `_encodeWorker` null so the page encodes on its own thread.
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
        if (this._encodeWorker === worker) this._encodeWorker = null;
        try { worker.terminate(); } catch (e) { /* ignore */ }
        done();
      };
      const timer = setTimeout(() => drop("probe timeout"), 3000);
      worker.onmessage = (e) => {
        const m = e.data;
        if (m.type === "probed") {
          clearTimeout(timer);
          this._encodeWorker = worker;
          done();
          return;
        }
        if (m.type === "ready") { this._encoderCodecName = m.codec; return; }
        if (m.type === "chunk") {
          if (this._active && this._generation === generation) {
            this._deliverEncoded(m.codecId, m.keyframe, new Uint8Array(m.buffer));
          }
          return;
        }
        if (m.type === "unsupported" || m.type === "error") {
          clearTimeout(timer);
          // Before the page commits, fall back to page encoding; after (a codec
          // dropped out mid-stream), the next frame re-routes to the page too.
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

  // Tear down the encode worker (idempotent); the page-thread encoder takes over.
  _stopEncodeWorker() {
    const worker = this._encodeWorker;
    this._encodeWorker = null;
    if (worker) {
      try { worker.postMessage({ type: "stop" }); } catch (e) { /* ignore */ }
      setTimeout(() => { try { worker.terminate(); } catch (e) { /* ignore */ } }, 100);
    }
  }

  // --- frame sources ------------------------------------------------------

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
      // Worker-only MediaStreamTrackProcessor means Safari: raw sensor frames
      // with no readable metadata, so their rotation is derived per frame.
      this._deriveOrientation = !HAS_FRAME_ORIENTATION;
      this._logPath("capture: MediaStreamTrackProcessor in a worker");
      return worker;
    }
    this._stopEncodeWorker();
    this._candidateIndex = ENCODER_CANDIDATES.length;
    this._logPath("capture: <video> element sampled with requestVideoFrameCallback (JPEG)");
    return this._videoSource(track, generation);
  }

  // MediaStreamTrackProcessor on the page: drain at camera cadence, never queue.
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

  // Standard mediacapture-transform: the processor only exists in workers, so a
  // clone of the track is transferred in and VideoFrames are transferred back.
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

  // <video> sampled with requestVideoFrameCallback; the element must stay in the
  // DOM (visually inert) or engines stop decoding for it. The element itself is
  // handed over: this source only runs with the JPEG rung pinned, and drawImage
  // both reads it and bakes any orientation the engine knows.
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
    const sample = () => {
      if (this._generation !== generation) {
        return;
      }
      if (video.readyState >= 2 && video.videoWidth > 0) {
        this._handleFrame(video);
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

  // --- encoding -----------------------------------------------------------

  // Every source lands here with one VideoFrame the receiver owns (or, on the
  // pinned JPEG rung, the <video> element); a frame is always closed.
  _handleFrame(frame) {
    if (!this._active || !this._canSend()) {
      closeFrame(frame);
      return;
    }
    const now = performance.now();
    if (now - this._lastSendMs < 1000 / this.fps - 1) {
      closeFrame(frame);
      return;
    }
    if (this._encodeWorker) {
      // Hand the frame to the encode worker (transferred, zero-copy). It only
      // takes VideoFrames; a <video>-element frame (non-transferable) or a dead
      // worker throws, so drop back to encoding on this thread from here.
      try {
        this._encodeWorker.postMessage({ type: "frame", frame }, [frame]);
        this._logPath("encode: VideoEncoder in a worker (frames from the page)");
        this._lastSendMs = now;
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
    const rotation = this._deriveOrientation ? deriveRotation() : (frame.rotation || 0);
    const flip = this._deriveOrientation ? false : !!frame.flip;
    const s = this._encodedSize;
    if (!this._encoder || !s || s.w !== w || s.h !== h || s.rotation !== rotation || s.flip !== flip) {
      // Size or orientation changed: a VideoEncoder latches the orientation of
      // its first frame and rejects any other, so it is rebuilt for both. One
      // rebuild runs at a time; frames racing it are dropped, never queued.
      if (this._configuring) {
        frame.close();
        return;
      }
      this._configuring = true;
      this._configureEncoder(w, h, rotation, flip)
        .then(() => this._encodeWith(frame, w, h, now))
        .finally(() => {
          this._configuring = false;
        });
      return;
    }
    this._encodeWith(frame, w, h, now);
  }

  _encodeWith(frame, w, h, now) {
    const encoder = this._encoder;
    if (!encoder || encoder.state !== "configured" || !this._active) {
      frame.close();
      return;
    }
    if (encoder.encodeQueueSize > 1) {
      // The encoder is behind: drop rather than queue latency.
      frame.close();
      return;
    }
    const keyFrame = this._forceKeyframe || now - this._lastKeyframeMs >= KEYFRAME_INTERVAL_MS;
    try {
      encoder.encode(frame, { keyFrame });
      this._lastSendMs = now;
      if (keyFrame) {
        this._lastKeyframeMs = now;
        this._forceKeyframe = false;
      }
    } catch (error) {
      this._onEncoderFailure(error);
    }
    frame.close();
  }

  // Builds the encoder for the first candidate the engine supports at w x h,
  // stamping the orientation its frames carry onto every chunk it emits.
  // Resolves when an encoder is configured or every candidate was rejected (then
  // frames take the JPEG path).
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
          output: (chunk) => this._onChunk(cand, chunk, generation, rotation, flip),
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

  // One encoded frame out to the transport. When the socket is backed up the frame
  // is dropped and the next one forced to a keyframe: the server's decoder must never
  // get a delta built on a frame it never received. Independent JPEG frames, which
  // carry no such dependency, do not go through here.
  _deliverEncoded(codecId, keyframe, bytes) {
    if (!this._canSend()) {
      if (!this._chainBroken) {
        this._chainBroken = true;
        this.requestKeyframe();
      }
      return;
    }
    if (this._chainBroken && !keyframe) {
      this.requestKeyframe();
      return;
    }
    this._sendFrame(codecId, keyframe, bytes);
    if (keyframe) {
      this._chainBroken = false;
    }
  }

  _onChunk(cand, chunk, generation, rotation, flip) {
    if (this._generation !== generation || !this._active) {
      return;
    }
    const buf = new Uint8Array(chunk.byteLength);
    chunk.copyTo(buf);
    this._sendFrame(cand.id, chunk.type === "key", buf, rotation, flip);
  }

  // An encoder that fails after reporting support (Firefox does this for H.264)
  // is abandoned for the next candidate; the JPEG path is the final fallback.
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

  // JPEG through OffscreenCanvas.convertToBlob: one encode in flight at a time.
  _encodeJpeg(frame, now) {
    if (this._jpegBusy) {
      closeFrame(frame);
      return;
    }
    // drawImage applies the frame's orientation metadata, so the JPEG leaves
    // upright (the canvas swaps dimensions for sideways frames) and carries no
    // orientation on the wire.
    const sideways = ((frame.rotation || 0) % 180) === 90;
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
      this._ctx.drawImage(frame, 0, 0, w, h);
    } catch (error) {
      closeFrame(frame);
      return;
    }
    closeFrame(frame);
    this._jpegBusy = true;
    this._lastSendMs = now;
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
