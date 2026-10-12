/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Adversarial test only: hold PNG bitmap decoding without changing video sinks.
 * The natural-route phase leaves the gate open. The test then holds just PNG
 * createImageBitmap calls to make cancellation observable, and releases every
 * held promise. VideoFrame, VideoDecoder, codec negotiation, and rendering
 * selection remain unchanged. During the scene race, tracked PNG bitmaps also
 * report successful drawImage calls and close calls; originals always execute.
 * The test requires a positive paint observation as well as rejecting an old
 * bitmap. This instrumentation is not a latency benchmark.
 * @module
 */
(() => {
  const records = [];
  const workers = new Set();
  let holding = false, observing = false, width = 0, height = 0;
  const record = event => records.push({ at: performance.now(), ...event });
  /**
   * Instrument only the selected PNG dimensions in this page or video worker.
   * @param {function(object): void} notify Record one PNG lifecycle event.
   * @returns {Function} Set hold/observation state and release pending decodes.
   */
  function installPngGate(notify) {
    const original = globalThis.createImageBitmap;
    if (typeof original !== 'function') return () => {};
    let hold = false, observe = false, next = 0, targetWidth = 0, targetHeight = 0;
    let trackedCount = 0;
    const tracked = new WeakMap();
    const pending = new Map();
    for (const Context of [globalThis.CanvasRenderingContext2D, globalThis.OffscreenCanvasRenderingContext2D]) {
      if (!Context) continue;
      const originalDraw = Context.prototype.drawImage;
      Context.prototype.drawImage = function (...args) {
        const result = originalDraw.apply(this, args);
        if (trackedCount && tracked.has(args[0]))
          notify({ kind: 'bitmap-painted', id: tracked.get(args[0]) });
        return result;
      };
    }
    if (globalThis.ImageBitmap) {
      const originalClose = ImageBitmap.prototype.close;
      ImageBitmap.prototype.close = function (...args) {
        const id = tracked.get(this);
        const result = originalClose.apply(this, args);
        if (id !== undefined) {
          tracked.delete(this);
          trackedCount--;
          notify({ kind: 'bitmap-closed', id });
        }
        return result;
      };
    }
    globalThis.createImageBitmap = function (...args) {
      if ((!hold && !observe) || !(args[0] instanceof Blob) || args[0].type !== 'image/png')
        return original.apply(this, args);
      return args[0].slice(0, 24).arrayBuffer().then(header => {
        if ((!hold && !observe) || header.byteLength !== 24) return original.apply(this, args);
        const view = new DataView(header);
        const width = view.getUint32(16), height = view.getUint32(20);
        if (view.getUint32(0) !== 0x89504e47 || view.getUint32(12) !== 0x49484452
            || width !== targetWidth || height !== targetHeight) return original.apply(this, args);
        const id = ++next;
        const decode = () => Promise.resolve(original.apply(this, args)).then(bitmap => {
          tracked.set(bitmap, id);
          trackedCount++;
          notify({ kind: 'bitmap-created', id, width, height });
          return bitmap;
        });
        if (!hold) {
          notify({ kind: 'decode-observed', id, width, height, bytes: args[0].size });
          return decode();
        }
        notify({ kind: 'decode-held', id, width, height, bytes: args[0].size });
        return new Promise(resolve => pending.set(id, resolve)).then(() => {
          pending.delete(id);
          notify({ kind: 'decode-released', id, width, height });
          return decode();
        });
      });
    };
    return (value, width, height, observing = false) => {
      hold = !!value;
      observe = !!observing;
      targetWidth = width;
      targetHeight = height;
      if (!hold) for (const resolve of pending.values()) resolve();
    };
  }
  const pageGate = installPngGate(event => record({ thread: 'page', ...event }));
  const prefix = `(() => {
    const gate = (${installPngGate.toString()})(event => self.postMessage({
      type: '__refinementProbeDecode', ...event}));
    self.addEventListener('message', event => {
      if(event.data?.type === '__refinementProbeControl') gate(event.data.hold, event.data.width, event.data.height, event.data.observe);
    });
  })();\n`;
  const createURL = URL.createObjectURL.bind(URL);
  URL.createObjectURL = object => createURL(object instanceof Blob && /javascript/.test(object.type)
    ? new Blob([prefix, object], {type: object.type}) : object);
  const NativeWorker = window.Worker;
  window.Worker = class extends NativeWorker {
    constructor(...args) {
      super(...args);
      workers.add(this);
      this.addEventListener('message', event => {
        if (event.data?.type === '__refinementProbeDecode')
          record({thread: 'worker', ...event.data});
      });
      this.postMessage({type: '__refinementProbeControl', hold: holding, observe: observing, width, height});
    }
    terminate() { workers.delete(this); return super.terminate(); }
  };
  window.__refinementProbeDecodes = records;
  window.__setRefinementPngHold = (value, w = 0, h = 0, observe = false) => {
    holding = !!value; observing = !!observe; width = w; height = h;
    pageGate(holding, width, height, observing);
    for(const worker of workers) worker.postMessage({type:'__refinementProbeControl',hold:holding,observe:observing,width,height});
  };
})();
