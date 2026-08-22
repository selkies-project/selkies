// Transport-neutral webcam capture: getUserMedia video frames are encoded as
// JPEG and handed to a transport-supplied sender. Both the WebSocket and WebRTC
// cores reuse this, supplying their own framing.
//
// The primary path reads VideoFrames directly from the track with
// MediaStreamTrackProcessor (no <video> element, no requestVideoFrameCallback),
// pacing off the real camera cadence and dropping frames under load. Firefox
// lacks MediaStreamTrackProcessor, so a <video>+canvas fallback keeps it working.
// JPEG encoding always goes through OffscreenCanvas.convertToBlob — the only
// still-image encoder the browser exposes.
//
// Capture follows tab visibility: a hidden tab halts the frame pump (the
// camera track itself stays open so no permission re-prompt is needed) and a
// visible tab rebuilds it from the live track, so nothing queued while the
// tab was away is ever sent and the first frame after resume is current.

export class WebcamCapture {
  // opts:
  //   sendFrame(Uint8Array)  required, delivers one encoded JPEG frame
  //   onStateChange(active)  optional, called when capture starts/stops
  //   onError(error)         optional, surfaces getUserMedia/encode failures
  //   canSend()              optional, returns false to skip a frame (backpressure)
  //   width, height, fps, quality
  constructor(opts) {
    this._sendFrame = opts.sendFrame;
    this._onStateChange = opts.onStateChange || (() => {});
    this._onError = opts.onError || (() => {});
    this._canSend = opts.canSend || (() => true);
    this.width = opts.width || 1280;
    this.height = opts.height || 720;
    this.fps = opts.fps || 30;
    this.quality = opts.quality || 0.8;

    this._stream = null;
    this._reader = null;
    this._video = null;
    this._timer = null;
    this._canvas = null;
    this._ctx = null;
    this._encoding = false;
    this._active = false;
    this._pumping = false;
    this._lastSendMs = 0;
    this._onVisibilityChange = () => {
      if (!this._active) {
        return;
      }
      if (document.hidden) {
        this._haltPump();
      } else {
        this._pump();
      }
    };
  }

  get active() {
    return this._active;
  }

  async start(deviceId) {
    if (this._active) {
      return;
    }
    if (!navigator.mediaDevices || !navigator.mediaDevices.getUserMedia) {
      this._onError(new Error("getUserMedia unavailable"));
      return;
    }
    const videoConstraints = {
      width: { ideal: this.width },
      height: { ideal: this.height },
      frameRate: { ideal: this.fps },
    };
    if (deviceId) {
      videoConstraints.deviceId = { exact: deviceId };
    }
    try {
      this._stream = await navigator.mediaDevices.getUserMedia({
        video: videoConstraints,
        audio: false,
      });
    } catch (error) {
      this._onError(error);
      return;
    }

    const track = this._stream.getVideoTracks()[0];
    if (track && track.getSettings) {
      const settings = track.getSettings();
      if (settings.width) this.width = settings.width;
      if (settings.height) this.height = settings.height;
    }

    this._canvas = new OffscreenCanvas(this.width, this.height);
    this._ctx = this._canvas.getContext("2d", { alpha: false, desynchronized: true });

    this._active = true;
    this._onStateChange(true);
    document.addEventListener("visibilitychange", this._onVisibilityChange);
    if (!document.hidden) {
      await this._pump();
    }
  }

  // Starts the frame source for the live track; idempotent while pumping.
  async _pump() {
    if (this._pumping || !this._active || !this._stream) {
      return;
    }
    this._pumping = true;
    this._lastSendMs = 0;
    const track = this._stream.getVideoTracks()[0];
    if (typeof MediaStreamTrackProcessor !== "undefined" && track) {
      this._pumpProcessor(track);
    } else {
      // Firefox and any engine without MediaStreamTrackProcessor.
      await this._startVideoFallback();
    }
  }

  // Stops the frame source without touching the camera track. An encode
  // already in flight is discarded by the pumping check in _encodeFrom's
  // completion, so no frame captured before the halt is sent after it.
  _haltPump() {
    this._pumping = false;
    if (this._reader) {
      try {
        this._reader.cancel();
      } catch (e) {
        /* ignore */
      }
      this._reader = null;
    }
    if (this._timer) {
      clearInterval(this._timer);
      this._timer = null;
    }
    if (this._video) {
      this._video.srcObject = null;
      this._video = null;
    }
  }

  // Draws one VideoFrame to the canvas and kicks off a non-blocking JPEG encode.
  // The frame is drawn and encoding flagged synchronously so the caller can
  // close the frame immediately; only one encode runs at a time.
  _encodeFrom(source, w, h) {
    if (this._encoding || !this._pumping || !this._canSend()) {
      return;
    }
    const now = performance.now();
    if (now - this._lastSendMs < 1000 / this.fps - 1) {
      return;
    }
    this._lastSendMs = now;
    if (this._canvas.width !== w || this._canvas.height !== h) {
      this._canvas.width = w;
      this._canvas.height = h;
    }
    try {
      this._ctx.drawImage(source, 0, 0, w, h);
    } catch (error) {
      return;
    }
    this._encoding = true;
    this._canvas
      .convertToBlob({ type: "image/jpeg", quality: this.quality })
      .then((blob) => blob.arrayBuffer())
      .then((buf) => {
        if (this._active && this._pumping) {
          this._sendFrame(new Uint8Array(buf));
        }
      })
      .catch((error) => this._onError(error))
      .finally(() => {
        this._encoding = false;
      });
  }

  async _pumpProcessor(track) {
    let processor;
    let reader;
    try {
      processor = new MediaStreamTrackProcessor({ track });
      reader = processor.readable.getReader();
      this._reader = reader;
    } catch (error) {
      // Fall back if construction fails despite the API being present.
      this._reader = null;
      await this._startVideoFallback();
      return;
    }
    // The loop is keyed on its own reader so a halt-then-resume, which
    // installs a new reader, ends this loop instead of racing two of them.
    while (this._active && this._reader === reader) {
      let result;
      try {
        result = await reader.read();
      } catch (error) {
        break;
      }
      if (result.done) {
        break;
      }
      const frame = result.value;
      if (!frame) {
        continue;
      }
      // Encoding never blocks the read loop, so frames keep draining at the
      // camera rate and stale ones are dropped rather than queued.
      this._encodeFrom(frame, frame.displayWidth, frame.displayHeight);
      frame.close();
    }
  }

  async _startVideoFallback() {
    const video = document.createElement("video");
    video.muted = true;
    video.playsInline = true;
    video.srcObject = this._stream;
    this._video = video;
    try {
      await video.play();
    } catch (error) {
      // A muted local stream should autoplay; if not, the loop tolerates a
      // not-yet-ready video by skipping frames.
    }
    if (this._video !== video) {
      // Halted while play() was pending.
      return;
    }
    const tick = () => {
      if (!this._active || this._video !== video) {
        return;
      }
      if (video.readyState >= 2) {
        this._encodeFrom(
          video,
          video.videoWidth || this.width,
          video.videoHeight || this.height,
        );
      }
    };
    if (video.requestVideoFrameCallback) {
      const onFrame = () => {
        if (!this._active || this._video !== video) return;
        tick();
        video.requestVideoFrameCallback(onFrame);
      };
      video.requestVideoFrameCallback(onFrame);
    } else {
      this._timer = setInterval(tick, 1000 / this.fps);
    }
  }

  stop() {
    if (!this._active && !this._stream) {
      return;
    }
    this._active = false;
    document.removeEventListener("visibilitychange", this._onVisibilityChange);
    this._haltPump();
    if (this._stream) {
      this._stream.getTracks().forEach((t) => {
        try {
          t.stop();
        } catch (e) {
          /* ignore */
        }
      });
      this._stream = null;
    }
    this._canvas = null;
    this._ctx = null;
    this._onStateChange(false);
  }
}
