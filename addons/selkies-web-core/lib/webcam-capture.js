// Webcam capture for the WebSocket transport: getUserMedia video frames are
// encoded in the page with WebCodecs (H.264 first, VP8 second) and handed to a
// transport-supplied sender one encoded frame at a time; the server's virtual
// camera decodes them. Engines without WebCodecs (no VideoEncoder, or no
// VideoFrame at all) fall back to JPEG frames drawn from a canvas — the <video>
// element itself when VideoFrame is missing — which the server passes on as an
// MJPEG device or decodes just the same. The WebRTC
// transport does not use this class: it attaches the camera track to a
// sendonly transceiver (lib/webrtc.js setWebcam) and the browser's own encoder
// takes over.
//
// Frames are read off the track through the first of these that the engine
// offers: MediaStreamTrackProcessor on the page (Chromium), the standard
// worker-only MediaStreamTrackProcessor with the track transferred into a
// DedicatedWorker (Safari 18+), and a <video> element sampled with
// requestVideoFrameCallback into VideoFrames (Firefox and anything else).

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

export class WebcamCapture {
  // opts:
  //   sendFrame(codecId, keyframe, Uint8Array)  required, delivers one encoded frame
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
    this._lastKeyframeMs = 0;
    this._lastSendMs = 0;
    this._canvas = null;
    this._ctx = null;
    this._jpegBusy = false;
    this._active = false;
    this._generation = 0;
  }

  get active() {
    return this._active;
  }

  // Name of the codec frames are sent as ("h264", "vp8", "mjpeg") or null.
  get codec() {
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
    const generation = ++this._generation;
    track.addEventListener("ended", () => {
      if (this._generation === generation) {
        this.stop();
      }
    });
    this._onStateChange(true);
    this._source = await this._openSource(track, generation);
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
  }

  stop() {
    if (!this._active && !this._stream) {
      return;
    }
    this._generation++;
    this._active = false;
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
    this._onStateChange(false);
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
      this._logPath("capture: MediaStreamTrackProcessor in a worker");
      return worker;
    }
    this._logPath("capture: <video> element sampled with requestVideoFrameCallback");
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
  // DOM (visually inert) or engines stop decoding for it.
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
            ctx = canvas.getContext("2d", { desynchronized: true, willReadFrequently: true });
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

  // --- encoding -----------------------------------------------------------

  // Every source lands here with one VideoFrame the receiver owns (or, without
  // WebCodecs, the <video> element); a frame is always closed.
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
    if (typeof VideoEncoder === "undefined" || this._candidateIndex >= ENCODER_CANDIDATES.length) {
      this._encodeJpeg(frame, now);
      return;
    }
    const w = frame.displayWidth || frame.codedWidth;
    const h = frame.displayHeight || frame.codedHeight;
    if (!this._encoder) {
      this._configureEncoder(w, h).then(() => this._encodeWith(frame, w, h, now));
      return;
    }
    if (this._encodedSize && (this._encodedSize.w !== w || this._encodedSize.h !== h)) {
      this._configureEncoder(w, h).then(() => this._encodeWith(frame, w, h, now));
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

  // Builds the encoder for the first candidate the engine supports at w x h.
  // Resolves when an encoder is configured or every candidate was rejected (then
  // frames take the JPEG path).
  async _configureEncoder(w, h) {
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
        this._encodedSize = { w, h };
        this._forceKeyframe = true;
        this._logPath(`encoder: ${cand.name} (${cand.codec}) at ${w}x${h}`);
        return;
      } catch (error) {
        this._candidateIndex++;
      }
    }
    this._encodedSize = null;
  }

  _onChunk(cand, chunk, generation) {
    if (this._generation !== generation || !this._active) {
      return;
    }
    const buf = new Uint8Array(chunk.byteLength);
    chunk.copyTo(buf);
    this._sendFrame(cand.id, chunk.type === "key", buf);
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
    const w = frame.displayWidth || frame.codedWidth || frame.videoWidth;
    const h = frame.displayHeight || frame.codedHeight || frame.videoHeight;
    if (!this._canvas) {
      this._logPath("encoder: JPEG through OffscreenCanvas (no usable VideoEncoder)");
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
