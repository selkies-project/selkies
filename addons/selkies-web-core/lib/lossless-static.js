/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Ordered, optional lossless stills for canvas and page-owned video sinks.
 *
 * A native scene identifies identical source pixels, not a decoder timestamp
 * or a period with no input. The socket pairs a versioned `lossless_sample`
 * preface with exactly one full-frame video payload. Its metadata follows the
 * chunk through decoding to the thread that owns presentation. A page-owned
 * track additionally matches compositor callbacks to submitted timestamps.
 * Unknown metadata never delays video and withdraws any refined image.
 *
 * A PNG may replace only the last presented scene, while no later scene is
 * announced. Both conditions are checked again after asynchronous decoding.
 * Subsequent video of that same scene still decodes; while the PNG is shown,
 * a canvas video sink retains one clone of its newest frame so disabling
 * restores the latest lossy reconstruction. An independent video sink keeps
 * presenting beneath its still and needs no clone. All clones, bitmaps, timers, and transfer buffers are
 * bounded and released on invalidation. One PNG decode may finish after
 * cancellation; at most one newer compressed PNG (64 MiB maximum) waits for
 * it, with all scene guards checked again before decoding. Neither this contract nor a canvas
 * draw asserts physical monitor presentation or native 10-bit precision.
 *
 * Factories are self-contained so the core can embed this module's source in
 * its socket and video workers without bundler-renamed external bindings.
 * @module
 */

/**
 * Validate the identity shared by a native sample and its PNG.
 * @param {*} value Untrusted protocol object.
 * @returns {boolean}
 */
export function validLosslessStamp(value) {
  const id = (v) => typeof v === 'string' && /^[1-9][0-9]{0,19}$/.test(v)
    && (v.length < 20 || v <= '18446744073709551615');
  const dim = (v) => Number.isInteger(v) && v > 0 && v <= 16384;
  return !!value && value.version === 1 && Number.isInteger(value.epoch)
    && value.epoch >= 0 && value.epoch <= 0xffffffff
    && ['run', 'source', 'scene', 'sample'].every((key) => id(value[key]))
    && dim(value.width) && dim(value.height)
    && value.width * value.height <= 16777216;
}

/**
 * Whether two stamps refer to identical source pixels and geometry.
 * @param {*} a
 * @param {*} b
 * @returns {boolean}
 */
export function sameLosslessScene(a, b) {
  return validLosslessStamp(a) && validLosslessStamp(b)
    && ['epoch', 'run', 'source', 'scene', 'width', 'height'].every((key) => a[key] === b[key]);
}

/**
 * Compare canonical unsigned decimal identities without losing u64 bits.
 * @param {string} a
 * @param {string} b
 * @returns {boolean}
 */
export function losslessSequenceAtLeast(a, b) {
  return a.length > b.length || (a.length === b.length && a >= b);
}

/**
 * Refuse a higher-precision or differently sized PNG before browser decoding.
 * The browser still validates the full stream and its CRCs; the header gate
 * prevents labeling an implicit 16-to-8 conversion as an exact 8-bit still.
 * @param {ArrayBuffer} png
 * @param {*} stamp
 * @returns {boolean}
 */
export function validLosslessPng(png, stamp) {
  if (!(png instanceof ArrayBuffer) || png.byteLength < 33 || png.byteLength > 67108864) return false;
  const bytes = new Uint8Array(png), view = new DataView(png);
  return [137, 80, 78, 71, 13, 10, 26, 10].every((value, i) => bytes[i] === value)
    && view.getUint32(8) === 13 && view.getUint32(12) === 0x49484452
    && view.getUint32(16) === stamp.width && view.getUint32(20) === stamp.height
    && bytes[24] === 8 && (bytes[25] === 2 || bytes[25] === 6)
    && bytes[26] === 0 && bytes[27] === 0 && bytes[28] <= 1;
}

/**
 * Pair metadata with video and assemble one bounded, sequential PNG.
 *
 * `receive` leaves ordinary messages alone. A video message consumes the
 * preceding metadata even when invalid, so a missing chunk cannot shift
 * identities onto a later chunk. Transfer control stays in this socket's
 * order, before either the page or the direct video-worker route sees it.
 * @param {object} [options]
 * @param {function(): number} [options.now] Monotonic milliseconds.
 * @param {Function} [options.setTimer] Testable timeout scheduler.
 * @param {Function} [options.clearTimer] Testable timeout cancellation.
 * @returns {{receive: Function, reset: Function, enable: Function, pendingBytes: Function}}
 */
export function createLosslessProtocol(options = {}) {
  const now = options.now || (() => performance.now());
  const schedule = options.setTimer || setTimeout;
  const unschedule = options.clearTimer || clearTimeout;
  let pending = null, transfer = null, timer = null, enabled = false, epoch = 0;
  let scene = null;
  const dropTransfer = () => {
    transfer = null;
    if (timer !== null) { unschedule(timer); timer = null; }
  };
  const clear = () => { pending = null; dropTransfer(); };
  // Only a socket reset may forget the server epoch. Disabling the feature
  // still has to reject older status and transfer messages already in flight.
  const reset = () => { clear(); epoch = 0; scene = null; };
  const enable = (value) => { enabled = !!value; if (!enabled) clear(); };
  const invalidate = (reason) => ({ handled: true, control: { type: 'lossless_invalidate', reason } });
  const u32 = (value) => Number.isInteger(value) && value >= 0 && value <= 0xffffffff;
  const receive = (data) => {
    if (!enabled && typeof data !== 'string') return null;
    if (transfer && now() - transfer.started >= 30000) dropTransfer();
    if (typeof data === 'string') {
      if (!data.startsWith('{') || !data.slice(0, 4096).includes('lossless_')) return null;
      let value;
      try { value = JSON.parse(data); } catch (_) { clear(); return invalidate('invalid-control'); }
      if (!value || typeof value.type !== 'string' || !value.type.startsWith('lossless_')) return null;
      if (data.length > 4096) { clear(); return invalidate('control-too-large'); }
      if (value.type === 'lossless_sample') {
        pending = null;
        if (!validLosslessStamp(value)) { dropTransfer(); return { handled: true }; }
        if (value.epoch < epoch) return { handled: true };
        if (value.epoch > epoch) { clear(); scene = null; epoch = value.epoch; }
        scene = value;
        pending = enabled ? value : null;
        if (transfer && !sameLosslessScene(transfer.stamp, value)) dropTransfer();
        return { handled: true };
      }
      if (value.type === 'lossless_status') {
        if (value.version !== 1 || !u32(value.epoch)) { clear(); return invalidate('invalid-status'); }
        if (value.epoch < epoch) return { handled: true };
        if (value.epoch > epoch) { clear(); scene = null; epoch = value.epoch; }
        if (!value.effective) clear();
        return { handled: true, control: value };
      }
      if (value.type === 'lossless_begin') {
        if (!enabled || (u32(value.epoch) && value.epoch < epoch)) return { handled: true };
        if (validLosslessStamp(value) && scene && !sameLosslessScene(scene, value)) return { handled: true };
        dropTransfer();
        if (!validLosslessStamp(value) || !u32(value.transferId)
            || !Number.isInteger(value.bytes) || value.bytes < 33 || value.bytes > 67108864) {
          return invalidate('invalid-transfer');
        }
        transfer = { stamp: value, id: value.transferId, buffer: new Uint8Array(value.bytes),
          offset: 0, started: now() };
        timer = schedule(dropTransfer, 30000);
        return { handled: true };
      }
      if (value.type === 'lossless_end') {
        // A canceled transfer can still finish a socket write. Its tail must
        // not retire the current scene's timer or a newer PNG assembly.
        if (!enabled || !transfer || value.transferId !== transfer.id
            || (u32(value.epoch) && value.epoch < epoch)) return { handled: true };
        const done = transfer;
        dropTransfer();
        if (value.version !== 1 || value.epoch !== done.stamp.epoch
            || done.offset !== done.buffer.length) {
          return invalidate('incomplete-transfer');
        }
        return { handled: true, png: done.buffer.buffer, stamp: done.stamp,
          transferId: done.id };
      }
      clear();
      return invalidate('unknown-control');
    }
    if (!(data instanceof ArrayBuffer) || data.byteLength === 0) return null;
    const bytes = new Uint8Array(data);
    if (bytes[0] === 0x0a) {
      if (!enabled) return { handled: true };
      if (!transfer) return { handled: true };
      if (data.byteLength < 5) { dropTransfer(); return invalidate('invalid-chunk'); }
      const head = new DataView(data);
      if (head.getUint32(1) !== transfer.id) return { handled: true };
      if (data.byteLength < 10) { dropTransfer(); return invalidate('invalid-chunk'); }
      const offset = head.getUint32(5);
      const length = data.byteLength - 9;
      if (offset !== transfer.offset
          || offset + length > transfer.buffer.length) {
        dropTransfer();
        return invalidate('invalid-chunk');
      }
      transfer.buffer.set(bytes.subarray(9), offset);
      transfer.offset += length;
      return { handled: true };
    }
    if (bytes[0] !== 0x03 && bytes[0] !== 0x04) return null;
    const stamp = pending;
    pending = null;
    if (!stamp) return { handled: false, stamp: null };
    const head = new DataView(data);
    const matched = data.byteLength > 12 && bytes[0] === 0x04 && stamp.y === 0
      && head.getUint16(4) === 0 && head.getUint16(2) === stamp.frame_id
      && head.getUint16(6) === stamp.width && head.getUint16(8) === stamp.height
      && data.byteLength === stamp.payload_bytes;
    if (!matched) dropTransfer();
    return { handled: false, stamp: matched ? stamp : null };
  };
  return { receive, reset, enable, pendingBytes: () => transfer ? transfer.buffer.length : 0 };
}

/**
 * Own the refined image in the thread that draws the visible canvas.
 *
 * `draw` consumes no frame; its caller retains the existing close/transfer
 * contract. A clone is owned only while a PNG is visible. `offer` owns and
 * closes the decoded PNG. A canvas backup exists only until a newer decoded
 * frame can restore the normal image, and is never updated per video frame.
 * A request has one absolute 35-second deadline, including transfer and decode.
 * Failure is terminal for that scene so repeated video samples cannot retry it.
 * @param {object} options
 * @param {function(): *} options.canvas Visible canvas.
 * @param {function(*): void} options.paint Draw without closing the image.
 * @param {function(*): *} options.backup Copy the normal canvas once.
 * @param {function(ArrayBuffer): Promise<*>} options.decode Decode the PNG.
 * @param {function(*): void} options.send Request/cancel control.
 * @param {function(*): void} [options.changed] Renderer state notification.
 * @param {boolean} [options.independentVideo] Video keeps presenting beneath a
 *   separate still; draw receives only its dimensions and never retains it.
 * @param {function(): number} [options.now] Monotonic milliseconds.
 * @param {Function} [options.setTimer] Testable timer scheduler.
 * @param {Function} [options.clearTimer] Testable timer cancellation.
 * @returns {{configure: Function, observe: Function, draw: Function, offer: Function,
 *   invalidate: Function, reset: Function, status: Function}}
 */
export function createLosslessRenderer(options) {
  const schedule = options.setTimer || setTimeout;
  const unschedule = options.clearTimer || clearTimeout;
  const now = options.now || (() => performance.now());
  let enabled = false, epoch = 0, generation = 0, timer = null;
  let announced = null, presented = null, asked = null, highWater = null;
  let requestOpen = false, deadline = null, deadlineAt = 0;
  let bitmap = null, restoreFrame = null, backup = null, decoding = false, reason = 'disabled';
  let decodeGeneration = -1, queuedOffer = null;
  const status = () => ({ enabled, epoch, shown: !!bitmap, pending: requestOpen && !bitmap,
    reason, scene: presented ? presented.scene : null,
    retainedVideoFrames: restoreFrame ? 1 : 0, queuedPngBytes: queuedOffer ? queuedOffer.png.byteLength : 0, backupBytes: backup ? backup.width * backup.height * 4 : 0 });
  const changed = () => { if (options.changed) options.changed(status()); };
  const finishRequest = () => {
    requestOpen = false;
    deadlineAt = 0;
    if (deadline !== null) { unschedule(deadline); deadline = null; }
  };
  const expireRequest = () => {
    if (!requestOpen) return;
    finishRequest();
    queuedOffer = null;
    options.send({ ...asked, op: 'cancel', version: 1 });
    reason = 'request-timeout';
    changed();
  };
  const accepting = () => {
    if (requestOpen && now() >= deadlineAt) expireRequest();
    return requestOpen;
  };
  const close = (value) => { if (value && typeof value.close === 'function') value.close(); };
  const dropRestore = () => {
    close(restoreFrame); restoreFrame = null;
    if (backup) { backup.width = 0; backup.height = 0; backup = null; }
  };
  const withdraw = (restore) => {
    try {
      if (bitmap && restore) {
        const image = restoreFrame || backup;
        const canvas = options.canvas();
        const width = image && (image.displayWidth ?? image.width);
        const height = image && (image.displayHeight ?? image.height);
        if (image && canvas && canvas.width === width && canvas.height === height) options.paint(image);
      }
    } catch (_) {
      // A lost canvas must still release the decoder frame on disable.
    } finally {
      close(bitmap); bitmap = null;
      dropRestore();
    }
  };
  const invalidate = (why = 'invalidated', restore = true) => {
    generation++;
    queuedOffer = null;
    if (timer !== null) { unschedule(timer); timer = null; }
    finishRequest();
    if (asked) options.send({ ...asked, op: 'cancel', version: 1, epoch });
    asked = null; announced = null; presented = null;
    withdraw(restore);
    reason = why;
    changed();
  };
  const configure = (config) => {
    if (!Number.isInteger(config.epoch) || config.epoch < epoch) return;
    if (enabled === !!config.enabled && epoch === config.epoch) return;
    invalidate(config.enabled ? 'waiting-for-scene' : 'disabled');
    if (epoch !== config.epoch) highWater = null;
    enabled = !!config.enabled; epoch = config.epoch;
    changed();
  };
  const reset = () => {
    invalidate('connection-reset');
    enabled = false; epoch = 0; highWater = null;
  };
  const usable = (stamp) => enabled && validLosslessStamp(stamp) && stamp.epoch === epoch;
  const observe = (stamp) => {
    if (!enabled) return;
    if (!usable(stamp)) { invalidate('unknown-sample'); return; }
    if (highWater && (stamp.run !== highWater.run || stamp.source !== highWater.source
        || !losslessSequenceAtLeast(stamp.sample, highWater.sample)
        || !losslessSequenceAtLeast(stamp.scene, highWater.scene)
        || (stamp.sample === highWater.sample && !sameLosslessScene(stamp, highWater)))) {
      invalidate('out-of-order-sample');
      return;
    }
    highWater = stamp;
    if (announced && !sameLosslessScene(announced, stamp)) invalidate('new-scene');
    announced = stamp;
  };
  const arm = () => {
    if (timer !== null || asked || !presented || !sameLosslessScene(presented, announced)) return;
    const token = generation;
    timer = schedule(() => {
      timer = null;
      if (!enabled || token !== generation || !sameLosslessScene(presented, announced)) return;
      asked = presented;
      requestOpen = true;
      deadlineAt = now() + 35000;
      const request = asked;
      deadline = schedule(() => {
        if (token !== generation || asked !== request || !requestOpen) return;
        expireRequest();
      }, 35000);
      reason = 'requesting';
      options.send({ op: 'request', version: 1, epoch, run: asked.run, source: asked.source,
        scene: asked.scene, sample: asked.sample, width: asked.width, height: asked.height });
      changed();
    }, 500);
  };
  const draw = (frame, stamp) => {
    if (!enabled) { if (!options.independentVideo) options.paint(frame); return true; }
    const canvas = options.canvas();
    const exact = usable(stamp) && canvas && stamp.width === canvas.width && stamp.height === canvas.height
      && frame.displayWidth === stamp.width && frame.displayHeight === stamp.height;
    if (!exact || !sameLosslessScene(stamp, announced)) {
      const latest = announced;
      invalidate(exact ? 'superseded-sample' : 'unknown-sample', false);
      // An older decoder output must not forget a newer received scene that
      // is still queued for decoding and may be the final quiet picture.
      announced = latest;
      if (!options.independentVideo) options.paint(frame);
      return true;
    }
    if (presented && !sameLosslessScene(presented, stamp)) {
      const latest = announced;
      invalidate('new-scene', false);
      announced = latest;
    }
    presented = stamp;
    if (bitmap) {
      if (options.independentVideo) return true;
      try {
        const retained = !!restoreFrame;
        const next = frame.clone();
        close(restoreFrame); restoreFrame = next;
        if (backup) { backup.width = 0; backup.height = 0; backup = null; }
        if (!retained) changed();
        return false;
      } catch (_) {
        invalidate('frame-retention-failed', false);
        options.paint(frame);
        return true;
      }
    }
    if (!options.independentVideo) options.paint(frame);
    arm();
    return true;
  };
  const canOffer = (stamp) => usable(stamp) && accepting() && asked && sameLosslessScene(stamp, asked)
    && sameLosslessScene(stamp, presented) && sameLosslessScene(stamp, announced)
    && losslessSequenceAtLeast(stamp.sample, asked.sample);
  const offer = async (stamp, png) => {
    if (bitmap || !canOffer(stamp) || !validLosslessPng(png, stamp)) return false;
    if (decoding) {
      // Browser PNG decoding cannot be canceled. Retain only the latest
      // eligible new generation until that one outstanding decode finishes.
      if (decodeGeneration !== generation) {
        queuedOffer = { stamp, png };
        changed();
      }
      return false;
    }
    decoding = true;
    const token = generation;
    decodeGeneration = token;
    let decoded = null;
    try {
      decoded = await options.decode(png);
      const canvas = options.canvas();
      if (token !== generation || !canOffer(stamp) || !canvas
          || decoded.width !== stamp.width || decoded.height !== stamp.height
          || canvas.width !== stamp.width || canvas.height !== stamp.height) return false;
      if (!bitmap && !options.independentVideo) backup = options.backup(canvas);
      close(bitmap); bitmap = decoded; decoded = null;
      options.paint(bitmap);
      finishRequest();
      reason = 'refined';
      changed();
      return true;
    } catch (_) {
      if (token === generation && requestOpen) {
        finishRequest();
        withdraw(false);
        reason = 'png-decode-failed';
        changed();
      }
      return false;
    } finally {
      decoding = false;
      close(decoded);
      const next = queuedOffer;
      queuedOffer = null;
      if (next) void offer(next.stamp, next.png);
    }
  };
  return { configure, observe, draw, offer, invalidate, reset, status };
}

/**
 * Bind a separate still to the page generator's compositor notifications.
 *
 * Writing a VideoFrame only submits it to a track. Its timestamp must also
 * appear in requestVideoFrameCallback before that scene can request a PNG.
 * The bounded ledger owns metadata only; normal frames keep their existing
 * write/close contract, including backpressure drops. A missing timestamp or
 * changed geometry withdraws the still instead of guessing a nearby sample.
 * Submitted timestamps must increase across resets, as the page's chunk clock
 * does; duplicate identities cannot be disambiguated by a compositor callback.
 * Received scene changes retire the still before the page can write their
 * decoded frames. The video remains visible underneath, so withdrawal needs
 * neither a restoration frame nor a copy of the normal video.
 *
 * This adapter requires submission and scene observation on the same thread.
 * It does not associate a worker-owned generator through asynchronous page
 * messages, and compositor callbacks do not establish physical display time.
 * @param {object} options
 * @param {function(): *} options.video The page-owned HTML video sink.
 * @param {function(*): void} options.paint Paint only a decoded PNG.
 * @param {function(ArrayBuffer): Promise<*>} options.decode
 * @param {function(*): void} options.send
 * @param {function(*): void} [options.changed] Hide the overlay when not shown.
 * @param {function(): number} [options.now]
 * @param {Function} [options.setTimer]
 * @param {Function} [options.clearTimer]
 * @returns {{configure: Function, observe: Function, submitted: Function,
 *   offer: Function, invalidate: Function, reset: Function, status: Function}}
 */
export function createLosslessTrackRenderer(options) {
  const samples = new Map();
  const geometry = { width: 0, height: 0 };
  let enabled = false, epoch = 0, generation = 0, callback = null, video = null;
  let presentedTimestamp = null;
  let lastSubmittedTimestamp = -Infinity;
  const renderer = createLosslessRenderer({ ...options, independentVideo: true,
    canvas: () => geometry, backup: () => null,
    changed: (state) => {
      if (options.changed) options.changed({ ...state, presentedTimestamp });
    },
  });
  const status = () => ({ ...renderer.status(), presentedTimestamp,
    trackedSamples: samples.size, callbackPending: callback !== null });
  const cancel = () => {
    generation++;
    if (callback !== null && video) video.cancelVideoFrameCallback(callback);
    callback = null; video = null; samples.clear(); presentedTimestamp = null;
  };
  const watch = () => {
    if (!enabled || callback !== null || !video) return;
    const token = generation;
    callback = video.requestVideoFrameCallback((_now, metadata) => {
      if (token !== generation || !enabled) return;
      callback = null;
      const timestamp = Math.round(metadata.mediaTime * 1000000);
      const sample = Number.isSafeInteger(timestamp) ? samples.get(timestamp) : null;
      for (const key of samples.keys()) if (key <= timestamp) samples.delete(key);
      if (!sample || sample.width !== metadata.width || sample.height !== metadata.height) {
        presentedTimestamp = null;
        renderer.draw({ displayWidth: metadata.width, displayHeight: metadata.height }, null);
      } else {
        geometry.width = sample.width; geometry.height = sample.height;
        presentedTimestamp = timestamp;
        renderer.draw({ displayWidth: sample.width, displayHeight: sample.height }, sample.stamp);
      }
      watch();
    });
  };
  const configure = (config) => {
    if (!Number.isInteger(config.epoch) || config.epoch < epoch) return;
    const nextVideo = options.video();
    const nextEnabled = !!config.enabled && !!nextVideo
      && typeof nextVideo.requestVideoFrameCallback === 'function'
      && typeof nextVideo.cancelVideoFrameCallback === 'function';
    if (enabled === nextEnabled && epoch === config.epoch && (!enabled || video === nextVideo)) return;
    const replaced = enabled && nextEnabled && video !== nextVideo;
    cancel();
    enabled = nextEnabled; epoch = config.epoch; video = enabled ? nextVideo : null;
    if (replaced) renderer.invalidate('track-replaced');
    renderer.configure({ enabled, epoch });
    watch();
  };
  const submitted = (frame, stamp) => {
    if (!enabled) return;
    if (!Number.isSafeInteger(frame.timestamp) || frame.timestamp <= lastSubmittedTimestamp) {
      samples.clear(); presentedTimestamp = null;
      renderer.draw(frame, null);
      return;
    }
    lastSubmittedTimestamp = frame.timestamp;
    if (!validLosslessStamp(stamp)
        || stamp.epoch !== epoch || frame.displayWidth !== stamp.width || frame.displayHeight !== stamp.height) {
      renderer.draw(frame, null);
      return;
    }
    if (samples.size >= 64) samples.delete(samples.keys().next().value);
    samples.set(frame.timestamp, { stamp, width: frame.displayWidth, height: frame.displayHeight });
  };
  const invalidate = (reason) => {
    samples.clear(); presentedTimestamp = null;
    renderer.invalidate(reason);
  };
  const reset = () => {
    cancel(); enabled = false; epoch = 0;
    renderer.reset();
  };
  return { configure, observe: renderer.observe, submitted, offer: renderer.offer,
    invalidate, reset, status };
}
