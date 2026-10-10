/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/** Deterministic protocol and same-canvas ordering/lifetime checks. @module */
import { createLosslessProtocol, createLosslessRenderer, createLosslessTrackRenderer, validLosslessStamp, validLosslessPng } from '../../addons/selkies-web-core/lib/lossless-static.js';
import assert from 'node:assert/strict';

let passed = 0, failed = 0;
/** Run a check without suppressing failures in the process result. */
async function check(name, body) {
  try { await body(); passed++; console.log(`PASS [lossless-static] ${name}`); }
  catch (error) { failed++; console.log(`FAIL [lossless-static] ${name}: ${error.stack}`); }
}
const stamp = (changes = {}) => ({ version: 1, epoch: 3, run: '9007199254740993', source: '11',
  scene: '7', sample: '18446744073709551614', width: 64, height: 32, frame_id: 42, y: 0,
  payload_bytes: 13, ...changes });
const text = (type, changes = {}) => JSON.stringify({ ...stamp(), type, ...changes });
/** A deterministic scheduler whose pending work is observable. */
function timers() {
  const jobs = new Map(); let next = 0, clock = 0;
  return { setTimer: (f, delay) => { const id = ++next; jobs.set(id, { f, delay }); return id; },
    clearTimer: (id) => jobs.delete(id), count: () => jobs.size,
    now: () => clock, advance: (delay) => { clock += delay; },
    run: (delay) => { clock += delay; for (const [id, job] of [...jobs]) if (job.delay === delay) { jobs.delete(id); job.f(); } } };
}
/** Produce an existing full-frame wire header, without its encoder. */
function video(changes = {}) {
  const s = stamp(changes), data = new ArrayBuffer(s.payload_bytes), view = new DataView(data);
  view.setUint8(0, 4); view.setUint8(1, 1); view.setUint16(2, s.frame_id);
  view.setUint16(4, s.y); view.setUint16(6, s.width); view.setUint16(8, s.height);
  return data;
}
/** Only the PNG header is parsed here; the browser probe validates full decoding. */
function png() {
  const data = new Uint8Array(33), view = new DataView(data.buffer);
  data.set([137,80,78,71,13,10,26,10]); view.setUint32(8,13); view.setUint32(12,0x49484452);
  view.setUint32(16,64); view.setUint32(20,32); data[24]=8; data[25]=6;
  return data.buffer;
}
function chunk(id, offset, values) {
  const out = new Uint8Array(9 + values.length), view = new DataView(out.buffer);
  out[0] = 10; view.setUint32(1, id); view.setUint32(5, offset); out.set(values, 9); return out.buffer;
}
/** Fake owned decoder frames distinguish the newest reconstruction and every close. */
function frame(label) {
  return { label, displayWidth: 64, displayHeight: 32, width: 64, height: 32, closed: 0, clones: [],
    close() { this.closed++; }, clone() { const copy = frame(label); this.clones.push(copy); return copy; } };
}
function rig() {
  const t = timers(), paints = [], sends = [], bitmaps = [], backups = [], statuses = [];
  let resolveDecode = null, failDecode = false;
  const canvas = { width: 64, height: 32 };
  const renderer = createLosslessRenderer({ canvas: () => canvas,
    paint: (image) => paints.push(image.label),
    backup: () => { const b = { width: 64, height: 32, label: paints.at(-1) }; backups.push(b); return b; },
    decode: async () => { if (failDecode) throw Error('PNG decode failed'); const b = frame('png'); bitmaps.push(b); if (resolveDecode) await resolveDecode; return b; },
    send: (message) => sends.push(message), changed: (status) => statuses.push(status), ...t });
  return { renderer, paints, sends, bitmaps, backups, statuses, t, canvas,
    failDecode: () => { failDecode = true; },
    defer: () => { let done; resolveDecode = new Promise((r) => { done = r; }); return done; },
    ready: () => { renderer.configure({ enabled: true, epoch: 3 }); renderer.observe(stamp()); renderer.draw(frame('video-1'), stamp()); t.run(500); } };
}

await check('u64 identifiers are exact strings; unsafe numbers and overflow are refused', () => {
  assert(validLosslessStamp(stamp()));
  for (const value of [Number('9007199254740993'), '18446744073709551616', '-1', '01', 'NaN', null]) {
    assert.equal(validLosslessStamp(stamp({ sample: value })), false);
  }
});
await check('off allocates no PNG buffer or metadata', () => {
  const p = createLosslessProtocol();
  p.receive(text('lossless_sample'));
  assert.equal(p.receive(video()), null);
  p.receive(text('lossless_begin', { transferId: 2, bytes: 67108864 }));
  assert.equal(p.pendingBytes(), 0);
});
await check('full-frame pairing consumes one preface and preserves u64 identifiers', () => {
  const p = createLosslessProtocol(); p.enable(true);
  p.receive(text('lossless_sample')); p.receive('ordinary-control');
  assert.equal(p.receive(video()).stamp.run, '9007199254740993');
  assert.equal(p.receive(video()).stamp, null);
});
await check('wrong geometry, id, stripe, and payload length cannot inherit metadata', () => {
  for (const change of [{ width: 62 }, { frame_id: 43 }, { y: 1 }, { payload_bytes: 14 }]) {
    const p = createLosslessProtocol(); p.enable(true); p.receive(text('lossless_sample'));
    assert.equal(p.receive(video(change)).stamp, null);
    assert.equal(p.receive(video()).stamp, null);
  }
});
await check('a PNG requires contiguous offsets and a matching completed end', () => {
  const t = timers(), p = createLosslessProtocol(t); p.enable(true);
  p.receive(text('lossless_begin', { transferId: 2, bytes: 33 }));
  p.receive(chunk(2, 0, new Uint8Array(16).fill(17)));
  p.receive(chunk(2, 16, new Uint8Array(17).fill(31)));
  const done = p.receive(text('lossless_end', { transferId: 2 }));
  assert.equal(done.png.byteLength, 33); assert.equal(new Uint8Array(done.png)[32], 31);
  assert.equal(p.pendingBytes(), 0); assert.equal(t.count(), 0);
  p.receive(text('lossless_begin', { transferId: 3, bytes: 33 }));
  assert.equal(p.receive(chunk(3, 1, new Uint8Array(32))).control.reason, 'invalid-chunk');
  assert.equal(p.pendingBytes(), 0);
});
await check('transfer timeout, disable, and a newer scene release the entire allocation', () => {
  const t = timers(), p = createLosslessProtocol(t); p.enable(true);
  p.receive(text('lossless_begin', { transferId: 2, bytes: 33 })); t.run(30000);
  assert.equal(p.pendingBytes(), 0);
  p.receive(text('lossless_begin', { transferId: 2, bytes: 33 })); p.enable(false);
  assert.equal(p.pendingBytes(), 0); assert.equal(t.count(), 0);
  p.enable(true); p.receive(text('lossless_begin', { transferId: 2, bytes: 33 }));
  p.receive(text('lossless_sample', { scene: '8' })); assert.equal(p.pendingBytes(), 0);
});
await check('overlarge and incomplete transfers are explicit failures', () => {
  const p = createLosslessProtocol(); p.enable(true);
  assert.equal(p.receive(text('lossless_begin', { transferId: 2, bytes: 67108865 })).control.reason, 'invalid-transfer');
  p.receive(text('lossless_begin', { transferId: 2, bytes: 33 }));
  assert.equal(p.receive(text('lossless_end', { transferId: 2 })).control.reason, 'incomplete-transfer');
});
await check('retired chunks and ends cannot invalidate a newer scene or transfer', () => {
  const p = createLosslessProtocol(timers()); p.enable(true);
  p.receive(text('lossless_begin', { transferId: 2, bytes: 33 }));
  p.receive(text('lossless_sample', { scene: '8' }));
  assert.equal(p.receive(chunk(2, 0, new Uint8Array(33))).control, undefined);
  assert.equal(p.receive(text('lossless_end', { transferId: 2 })).control, undefined);
  assert.equal(p.receive(video()).stamp.scene, '8');
  p.receive(text('lossless_begin', { transferId: 3, bytes: 33, scene: '8' }));
  p.receive(chunk(2, 0, new Uint8Array(33)));
  p.receive(text('lossless_end', { transferId: 2 }));
  assert.equal(p.pendingBytes(), 33);
  p.receive(chunk(3, 0, new Uint8Array(33)));
  assert.equal(p.receive(text('lossless_end', { transferId: 3 })).stamp.scene, '8');
});
await check('off-on and an older status preserve the new epoch assembly', () => {
  const p = createLosslessProtocol(timers()); p.enable(true);
  p.receive(text('lossless_status', { epoch: 3, effective: true }));
  p.receive(text('lossless_begin', { transferId: 2, bytes: 33 }));
  p.enable(false); p.enable(true);
  p.receive(text('lossless_status', { epoch: 4, effective: true }));
  p.receive(text('lossless_sample', { epoch: 4, scene: '8' }));
  assert.equal(p.receive(video()).stamp.epoch, 4);
  p.receive(text('lossless_begin', { transferId: 3, bytes: 33, epoch: 4, scene: '8' }));
  assert.equal(p.receive(text('lossless_status', { epoch: 3, effective: false })).control, undefined);
  p.receive(text('lossless_begin', { transferId: 2, bytes: 33 }));
  p.receive(chunk(2, 0, new Uint8Array(33)));
  p.receive(text('lossless_end', { transferId: 2 }));
  assert.equal(p.pendingBytes(), 33);
  p.receive(chunk(3, 0, new Uint8Array(33)));
  assert.equal(p.receive(text('lossless_end', { transferId: 3, epoch: 4 })).stamp.epoch, 4);
  p.reset(); p.receive(text('lossless_status', { epoch: 1, effective: true }));
  assert.equal(p.receive(text('lossless_status', { epoch: 0, effective: false })).control, undefined);
});
await check('disabled renderer draws normally without clones, timer, or request', () => {
  const r = rig(), f = frame('normal');
  r.renderer.observe(stamp()); r.renderer.draw(f, stamp());
  assert.deepEqual(r.paints, ['normal']); assert.equal(f.clones.length, 0);
  assert.equal(r.t.count(), 0); assert.equal(r.sends.length, 0);
});
await check('a request waits 500 ms from a presented scene, not from metadata', () => {
  const r = rig(); r.renderer.configure({ enabled: true, epoch: 3 }); r.renderer.observe(stamp());
  r.t.run(500); assert.equal(r.sends.length, 0);
  r.renderer.draw(frame('video'), stamp()); assert.equal(r.sends.length, 0);
  r.t.run(500); assert.equal(r.sends.length, 1); assert.equal(r.sends[0].op, 'request');
  r.renderer.observe(stamp({ sample: '18446744073709551615' }));
  r.renderer.draw(frame('same-scene'), stamp({ sample: '18446744073709551615' }));
  r.t.run(500); assert.equal(r.sends.length, 1);
});
await check('a silent request expires without retrying the same scene; a new scene recovers', async () => {
  const r = rig(); r.ready(); assert.equal(r.renderer.status().pending, true);
  r.t.run(35000);
  assert.equal(r.renderer.status().pending, false);
  assert.equal(r.renderer.status().reason, 'request-timeout');
  assert.equal(r.t.count(), 0);
  const cancel = r.sends.at(-1); assert.equal(cancel.op, 'cancel');
  for (const field of ['epoch', 'run', 'source', 'scene', 'sample', 'width', 'height'])
    assert.equal(cancel[field], stamp()[field]);
  r.renderer.observe(stamp()); r.renderer.draw(frame('same-scene'), stamp()); r.t.run(500);
  assert.equal(r.sends.filter(m => m.op === 'request').length, 1);
  assert.equal(await r.renderer.offer(stamp(), png()), false);
  const next = stamp({ scene: '8', sample: '18446744073709551615' });
  r.renderer.observe(next); r.renderer.draw(frame('new-scene'), next); r.t.run(500);
  assert.equal(await r.renderer.offer(next, png()), true);
  assert.equal(r.renderer.status().pending, false); assert.equal(r.renderer.status().shown, true);
  assert.equal(r.t.count(), 0);
});
await check('an expired partial transfer cannot leave the request pending or accept its late PNG', async () => {
  const r = rig(); r.ready();
  const t = timers(), p = createLosslessProtocol(t); p.enable(true);
  p.receive(text('lossless_begin', { transferId: 2, bytes: 33 }));
  p.receive(chunk(2, 0, new Uint8Array(16)));
  t.run(30000); assert.equal(p.pendingBytes(), 0);
  r.t.run(35000); assert.equal(r.renderer.status().pending, false);
  assert.equal(await r.renderer.offer(stamp(), png()), false);
  assert.equal(r.bitmaps.length, 0); assert.deepEqual(r.paints, ['video-1']);
});
await check('the absolute deadline rejects a decode before a delayed timeout callback runs', async () => {
  const r = rig(); r.ready(); const done = r.defer();
  const pending = r.renderer.offer(stamp(), png());
  r.t.advance(35001);
  done(); assert.equal(await pending, false);
  assert.equal(r.renderer.status().pending, false);
  assert.equal(r.renderer.status().reason, 'request-timeout');
  assert.equal(r.bitmaps[0].closed, 1); assert.deepEqual(r.paints, ['video-1']);
  assert.equal(r.t.count(), 0);
});
await check('later same-scene frames do not extend the absolute request deadline', async () => {
  const r = rig(); r.ready(); r.t.advance(30000);
  const next = stamp({ sample: '18446744073709551615' });
  r.renderer.observe(next); r.renderer.draw(frame('later-same-scene'), next);
  r.t.advance(5000);
  assert.equal(await r.renderer.offer(next, png()), false);
  assert.equal(r.renderer.status().reason, 'request-timeout'); assert.equal(r.bitmaps.length, 0);
});
await check('a deadline drops the queued PNG while an obsolete decode finishes', async () => {
  const r = rig(); r.ready(); const done = r.defer();
  const old = r.renderer.offer(stamp(), png());
  const next = stamp({ scene: '8', sample: '18446744073709551615' });
  r.renderer.observe(next); r.renderer.draw(frame('new-scene'), next); r.t.run(500);
  await r.renderer.offer(next, png()); assert.equal(r.renderer.status().queuedPngBytes, 33);
  r.t.run(35000);
  assert.equal(r.renderer.status().queuedPngBytes, 0); assert.equal(r.renderer.status().pending, false);
  done(); assert.equal(await old, false);
  assert.equal(r.bitmaps.length, 1); assert.equal(r.bitmaps[0].closed, 1);
  assert.deepEqual(r.paints, ['video-1', 'new-scene']);
});
await check('a PNG decode failure terminates pending state and releases its deadline', async () => {
  const r = rig(); r.ready(); r.failDecode();
  assert.equal(await r.renderer.offer(stamp(), png()), false);
  assert.equal(r.renderer.status().reason, 'png-decode-failed');
  assert.equal(r.renderer.status().pending, false); assert.equal(r.t.count(), 0);
  r.renderer.draw(frame('same-scene'), stamp()); r.t.run(500);
  assert.equal(r.sends.filter(m => m.op === 'request').length, 1);
});
await check('disable, epoch changes, and reset clear pending request deadlines', () => {
  for (const stop of [r => r.configure({ enabled: false, epoch: 3 }),
    r => r.configure({ enabled: true, epoch: 4 }), r => r.reset()]) {
    const r = rig(); r.ready(); stop(r.renderer);
    assert.equal(r.t.count(), 0); assert.equal(r.renderer.status().pending, false);
    const before = r.sends.length; r.t.run(35000); assert.equal(r.sends.length, before);
  }
});
await check('PNG is accepted only for the exact scene, dimensions, and a sample at least requested', async () => {
  const r = rig(); r.ready();
  for (const change of [{ scene: '6' }, { source: '12' }, { epoch: 4 }, { sample: '2' }, { width: 63 }]) {
    assert.equal(await r.renderer.offer(stamp(change), png()), false);
  }
  assert.equal(r.bitmaps.length, 0);
  assert.equal(await r.renderer.offer(stamp(), png()), true);
  assert.deepEqual(r.paints, ['video-1', 'png']);
});
await check('cancel carries the retired scene identity rather than canceling by epoch alone', () => {
  const r = rig(); r.ready();
  const next = stamp({ scene: '8', sample: '18446744073709551615' });
  r.renderer.observe(next); r.renderer.draw(frame('new-scene'), next); r.t.run(500);
  const canceled = r.sends.find(m => m.op === 'cancel');
  const requested = r.sends.filter(m => m.op === 'request').at(-1);
  for (const field of ['epoch', 'run', 'source', 'scene', 'sample', 'width', 'height'])
    assert.equal(canceled[field], stamp()[field]);
  assert.equal(requested.scene, '8'); assert.equal(requested.epoch, canceled.epoch);
});
await check('a newer unencoded quiet sample is eligible and only one PNG decode can be outstanding', async () => {
  const r = rig(); r.ready(); const done = r.defer();
  const pending = r.renderer.offer(stamp({ sample: '18446744073709551615' }), png());
  assert.equal(await r.renderer.offer(stamp(), png()), false);
  assert.equal(r.bitmaps.length, 1);
  done(); assert.equal(await pending, true);
  r.renderer.configure({ enabled: false, epoch: 2 });
  assert.equal(r.renderer.status().shown, true);
});
await check('later scene announced while PNG decodes prevents its commit and closes it', async () => {
  const r = rig(); r.ready(); const done = r.defer();
  const pending = r.renderer.offer(stamp(), png());
  r.renderer.observe(stamp({ scene: '8', sample: '18446744073709551615' }));
  done(); assert.equal(await pending, false);
  assert.deepEqual(r.paints, ['video-1']); assert.equal(r.bitmaps[0].closed, 1);
});
await check('a new scene PNG waits for an obsolete decode and still refines without a retry', async () => {
  const r = rig(); r.ready(); const done = r.defer();
  const old = r.renderer.offer(stamp(), png());
  const newer = stamp({scene:'8',sample:'18446744073709551615'});
  r.renderer.observe(newer); r.renderer.draw(frame('new-scene'), newer); r.t.run(500);
  assert.equal(await r.renderer.offer(newer, png()), false);
  assert.equal(r.renderer.status().queuedPngBytes, 33);
  assert.equal(r.statuses.at(-1).queuedPngBytes, 33);
  done(); assert.equal(await old, false);
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(r.renderer.status().shown, true);
  assert.equal(r.renderer.status().scene, '8');
  assert.equal(r.renderer.status().queuedPngBytes, 0);
  assert.equal(r.statuses.at(-1).queuedPngBytes, 0);
  assert.equal(r.bitmaps.length, 2); assert.equal(r.bitmaps[0].closed, 1);
});
await check('off while decoding prevents commit without delaying ordinary video', async () => {
  const r = rig(); r.ready(); const done = r.defer();
  const pending = r.renderer.offer(stamp(), png());
  r.renderer.configure({ enabled: false, epoch: 3 });
  r.renderer.draw(frame('new-video'), null); done();
  assert.equal(await pending, false); assert.deepEqual(r.paints, ['video-1', 'new-video']);
});
await check('off restores the latest lossy reconstruction and closes every retained object', async () => {
  const r = rig(); r.ready(); await r.renderer.offer(stamp(), png());
  const f2 = frame('paint-over-2'), f3 = frame('paint-over-3');
  assert.equal(r.renderer.draw(f2, stamp()), false);
  assert.equal(r.renderer.draw(f3, stamp()), false);
  assert.equal(f2.clones[0].closed, 1); assert.equal(f3.clones[0].closed, 0);
  assert.equal(r.backups[0].width, 0);
  r.renderer.configure({ enabled: false, epoch: 3 });
  assert.equal(r.paints.at(-1), 'paint-over-3');
  assert.equal(f3.clones[0].closed, 1); assert.equal(r.bitmaps[0].closed, 1);
  assert.equal(r.t.count(), 0);
});
await check('off before another decode restores the single initial canvas backup', async () => {
  const r = rig(); r.ready(); await r.renderer.offer(stamp(), png());
  r.renderer.configure({ enabled: false, epoch: 3 });
  assert.deepEqual(r.paints, ['video-1', 'png', 'video-1']); assert.equal(r.backups[0].width, 0);
});
await check('unknown metadata and resize never preserve a PNG over the next draw', async () => {
  for (const next of [null, stamp({ width: 128 })]) {
    const r = rig(); r.ready(); await r.renderer.offer(stamp(), png());
    r.renderer.draw(frame('unknown-video'), next);
    assert.equal(r.paints.at(-1), 'unknown-video'); assert.equal(r.bitmaps[0].closed, 1);
    assert.equal(r.renderer.status().shown, false);
  }
});
await check('an older decoder output cannot erase a newer announced quiet scene', () => {
  const r = rig(), old = stamp({sample:'1'}), newer=stamp({scene:'8',sample:'2'});
  r.renderer.configure({enabled:true,epoch:3});
  r.renderer.observe(old); r.renderer.observe(newer);
  r.renderer.draw(frame('old-decoder-output'),old);
  r.renderer.draw(frame('latest-quiet-output'),newer); r.t.run(500);
  assert.equal(r.sends.filter(m=>m.op==='request').length,1);
  assert.equal(r.sends.at(-1).scene,'8');
  assert.deepEqual(r.paints,['old-decoder-output','latest-quiet-output']);
});
await check('a decoder frame with mismatched dimensions cannot arm a request', () => {
  const r = rig(); r.renderer.configure({ enabled: true, epoch: 3 }); r.renderer.observe(stamp());
  const f = frame('wrong'); f.displayHeight = 16; r.renderer.draw(f, stamp());
  r.t.run(500); assert.equal(r.sends.filter(m => m.op === 'request').length, 0);
});
await check('PNG16, palette, malformed header, and wrong dimensions are rejected before decode', () => {
  assert(validLosslessPng(png(), stamp()));
  for (const [index, value] of [[0,0], [24,16], [25,3], [23,31]]) {
    const data = png(); new Uint8Array(data)[index] = value;
    assert.equal(validLosslessPng(data, stamp()), false);
  }
});
await check('old samples, another source within an epoch, and conflicting duplicate tags cannot roll back scene', () => {
  for (const old of [stamp({sample:'1'}), stamp({source:'12'}), stamp({scene:'6'}), stamp({scene:'8'})]) {
    const r = rig(); r.ready(); r.renderer.observe(old); r.renderer.draw(frame('late'), old);
    r.t.run(500); assert.equal(r.sends.filter(m => m.op === 'request').length, 1);
    assert.equal(r.renderer.status().shown, false);
  }
});

/** Execute the actual inline worker source with deterministic decoder/browser primitives. */
async function workerRig() {
  const { readFileSync } = await import('node:fs');
  const vm = await import('node:vm');
  const { createPresentMeter } = await import('../../addons/selkies-web-core/lib/present-meter.js');
  const { createStripeClock } = await import('../../addons/selkies-web-core/lib/stripe-clock.js');
  const { createChunkStamp } = await import('../../addons/selkies-web-core/lib/chunk-stamp.js');
  const source = readFileSync(new URL('../../addons/selkies-web-core/selkies-ws-core.js', import.meta.url), 'utf8');
  const start = source.indexOf('const VIDEO_WORKER_SRC = `') + 'const VIDEO_WORKER_SRC = '.length;
  const template = source.slice(start, source.indexOf('\n`;', start) + 2);
  const read = (file) => readFileSync(new URL('../../addons/selkies-web-core/lib/' + file, import.meta.url), 'utf8');
  const bindings = { losslessStaticSource: read('lossless-static.js'), decodeGateSource: read('decode-gate.js'),
    decodePaceSource: read('decode-pace.js'), wireCodecsSource: read('wire-codecs.js'), createPresentMeter,
    createStripeClock, createChunkStamp, isSkiaWebKit: () => false,
    STRIPE_DECODE_QUEUE_LIMIT: 8, JPEG_STRIPE_REORDER_WINDOW: 8 };
  const code = Function(...Object.keys(bindings), 'return ' + template)(...Object.values(bindings));
  const t = timers(), messages = [], paints = [], decoders = [];
  class Canvas {
    constructor(w, h) { this.width = w; this.height = h; this.label = 'blank'; }
    getContext() { return { drawImage: (image) => { this.label = image.label; paints.push(image.label); } }; }
  }
  class Decoder {
    constructor(options) { this.options = options; this.state = 'unconfigured'; this.decodeQueueSize = 0; decoders.push(this); }
    configure() { this.state = 'configured'; }
    decode(chunk) { this.last = chunk; }
    reset() { this.state = 'unconfigured'; }
    close() { this.state = 'closed'; }
    static async isConfigSupported() { return { supported: true }; }
  }
  const self = { postMessage: (value) => messages.push(value) };
  const context = { self, console, ArrayBuffer, Uint8Array, DataView, Map, Set, Blob,
    Promise, performance: { now: () => 100, timeOrigin: 0 },
    navigator: { userAgent: 'test', platform: 'test' },
    setTimeout: t.setTimer, clearTimeout: t.clearTimer, setInterval: () => 1, clearInterval: () => {},
    OffscreenCanvas: Canvas, VideoDecoder: Decoder,
    EncodedVideoChunk: class { constructor(options) { Object.assign(this, options); } },
    createImageBitmap: async () => frame('png'),
  };
  vm.runInNewContext(code, context);
  const post = (data) => self.onmessage({ data });
  post({ canvas: new Canvas(64, 32) });
  post({ type: 'decoderConfig', codec: 'avc1.42001e', codedWidth: 64, codedHeight: 32 });
  post({ type: 'losslessConfig', serial: 1, allowed: true, enabled: true, epoch: 3 });
  const decode = (label, identity, timestamp) => {
    post({ type: 'chunk', key: true, data: new ArrayBuffer(1), timestamp, frameId: timestamp,
      reference: timestamp, stamp: identity });
    const output = frame(label); output.timestamp = timestamp;
    decoders.at(-1).options.output(output);
    return output;
  };
  return { post, decode, messages, paints, t };
}
await check('actual video worker carries decoder timestamp identity to the canvas and restores newest frame', async () => {
  const r = await workerRig();
  const first = r.decode('first', stamp({ sample: '1' }), 1);
  assert.equal(first.closed, 1);
  r.t.run(500);
  assert.equal(r.messages.filter(m => m.type === 'losslessSend' && m.message.op === 'request').length, 1);
  r.post({ type: 'lossless', png: png(), stamp: stamp({ sample: '1' }), transferId: 8 });
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(r.paints.at(-1), 'png');
  const newer = r.decode('newest-reconstruction', stamp({ sample: '2' }), 2);
  assert.equal(newer.closed, 1); assert.equal(newer.clones.length, 1);
  r.post({ type: 'losslessConfig', serial: 2, allowed: false, enabled: false, epoch: 3 });
  assert.equal(r.paints.at(-1), 'newest-reconstruction'); assert.equal(newer.clones[0].closed, 1);
});
await check('a delayed config through another worker port cannot undo local disable', async () => {
  const r = await workerRig(); r.decode('first', stamp({sample:'1'}), 1); r.t.run(500);
  r.post({type:'losslessConfig',serial:2,allowed:false,enabled:false,epoch:3});
  r.post({type:'losslessConfig',serial:1,allowed:true,enabled:true,epoch:3});
  r.decode('ordinary', stamp({sample:'2'}), 2); r.t.run(500);
  assert.equal(r.messages.filter(m=>m.type==='losslessSend'&&m.message.op==='request').length,1);
  assert.equal(r.paints.at(-1),'ordinary');
});
await check('actual worker refuses unknown decoder output without stopping its video', async () => {
  const r = await workerRig();
  r.decode('first', stamp({ sample: '1' }), 1); r.t.run(500);
  r.post({ type: 'lossless', png: png(), stamp: stamp({ sample: '1' }), transferId: 8 });
  await new Promise(resolve => setImmediate(resolve));
  r.decode('unidentified', null, 2);
  assert.equal(r.paints.at(-1), 'unidentified');
  assert.equal(r.messages.filter(m => m.type === 'losslessSend' && m.message.op === 'cancel').length, 1);
});

/** Exercise the production socket worker without changing its routing logic. */
async function socketRig(divert) {
  const { readFileSync } = await import('node:fs');
  const vm = await import('node:vm');
  const source = readFileSync(new URL('../../addons/selkies-web-core/selkies-ws-core.js', import.meta.url), 'utf8');
  const start = source.indexOf('const SOCKET_WORKER_SRC = `') + 'const SOCKET_WORKER_SRC = '.length;
  const template = source.slice(start, source.indexOf('\n`;', start) + 2);
  const bindings = { losslessStaticSource: readFileSync(new URL('../../addons/selkies-web-core/lib/lossless-static.js', import.meta.url), 'utf8'),
    BUFFERED_DRAIN_MS: 25, ACK_HEARTBEAT_MS: 250, BACKPRESSURE_INTERVAL_MS: 25 };
  const code = Function(...Object.keys(bindings), 'return ' + template)(...Object.values(bindings));
  const messages = [], workerMessages = [], sockets = [], t = timers();
  class Socket { constructor() { this.readyState = 1; sockets.push(this); } send() {} }
  const self = { postMessage: value => messages.push(value) };
  vm.runInNewContext(code, { self, console, WebSocket: Socket, ArrayBuffer, Uint8Array, DataView,
    performance: { now: () => 100, timeOrigin: 0 }, Date,
    setTimeout: t.setTimer, clearTimeout: t.clearTimer, setInterval: () => 1, clearInterval: () => {} });
  const post = data => self.onmessage({ data });
  post({ type: 'losslessConfig', serial: 1, allowed: true, enabled: true, epoch: 3, workerCanvas: true });
  post({ type: 'videoPort', port: { postMessage: m => workerMessages.push(m) } });
  post({ type: 'videoState', divert, ack: false });
  post({ type: 'open', url: 'ws://private-test/' });
  const receive = data => sockets[0].onmessage({ data });
  return { post, receive, messages, workerMessages };
}
await check('actual socket worker keeps metadata with video and routes PNG to the canvas during warmup or divert', async () => {
  for (const divert of [false, true]) {
    const r = await socketRig(divert);
    r.receive(text('lossless_status', { effective: true }));
    r.receive(text('lossless_sample')); r.receive(video());
    const destination = divert ? r.workerMessages : r.messages;
    const packet = destination.find(m => m.stamp && (m.buffer || m.data));
    assert.equal(packet.stamp.run, stamp().run);
    r.receive(text('lossless_begin', { transferId: 2, bytes: 33 }));
    r.receive(chunk(2, 0, new Uint8Array(png())));
    r.receive(text('lossless_end', { transferId: 2 }));
    assert.equal(r.workerMessages.filter(m => m.png).length, 1);
    assert.equal(r.messages.filter(m => m.png).length, 0);
    assert.equal(r.messages.filter(m => m.control?.type === 'lossless_status').length, 1);
  }
});

await check('actual native-page socket pairs metadata before an outstanding gzip control', async () => {
  const {readFileSync}=await import('node:fs'); const vm=await import('node:vm');
  const source=readFileSync(new URL('../../addons/selkies-web-core/selkies-ws-core.js',import.meta.url),'utf8');
  const start=source.indexOf('  const onSocketMessage = (event) => {');
  const end=source.indexOf('\n  };',start)+5;
  const parser=createLosslessProtocol(timers()); parser.enable(true);
  const packets=[], controls=[];
  const ctx={websocket:{},pageLosslessProtocol:parser,receiveLossless:m=>controls.push(m),
    __rawWsMessage:m=>packets.push(m),__wsGzPending:1,__wsCtrlChain:Promise.resolve(),
    ArrayBuffer,Uint8Array,console};
  vm.runInNewContext(source.slice(start,end)+'\nthis.dispatch=onSocketMessage;',ctx);
  ctx.dispatch({data:text('lossless_status',{effective:true})});
  ctx.dispatch({data:text('lossless_sample')});ctx.dispatch({data:video()});
  assert.equal(packets.length,1);assert.equal(packets[0].stamp.sample,stamp().sample);
  ctx.dispatch({data:text('lossless_begin',{transferId:2,bytes:33})});
  ctx.dispatch({data:chunk(2,0,new Uint8Array(png()))});
  ctx.dispatch({data:text('lossless_end',{transferId:2})});
  assert.equal(controls.filter(m=>m.png).length,1);
  ctx.websocket={_worker:{}};
  ctx.dispatch({data:video(),stamp:stamp()});
  assert.equal(packets.at(-1).stamp.sample,stamp().sample);
});
await check('retired sockets cannot dispatch page control, frames, or close into a replacement', async () => {
  const {readFileSync}=await import('node:fs'); const vm=await import('node:vm');
  const source=readFileSync(new URL('../../addons/selkies-web-core/selkies-ws-core.js',import.meta.url),'utf8');
  const start=source.indexOf('  const bindSessionSocket = () => {');
  const end=source.indexOf('\n  };',start)+5;const old={},current={},called=[];
  const ctx={websocket:old,gzipTextSends:()=>{},onSocketOpen:()=>called.push('open'),
    onSocketMessage:()=>called.push('message'),onSocketError:()=>called.push('error'),onSocketClose:()=>called.push('close')};
  vm.runInNewContext(source.slice(start,end)+'\nthis.bind=bindSessionSocket;',ctx);ctx.bind();
  ctx.websocket=current;ctx.bind();
  for(const event of ['onopen','onmessage','onerror','onclose'])old[event]({});
  assert.equal(called.length,0);current.onmessage({});assert.deepEqual(called,['message']);
});

/** Drive compositor callbacks independently of submission and PNG decoding. */
function trackRig() {
  const t = timers(), paints = [], sends = [], bitmaps = [], statuses = [], videos = [];
  let decodeWait = null;
  const makeVideo = () => {
    let next = 0;
    const callbacks = new Map(), canceled = [];
    const video = {
      callbacks, canceled,
      requestVideoFrameCallback(fn) { const id = ++next; callbacks.set(id, fn); return id; },
      cancelVideoFrameCallback(id) { canceled.push(id); callbacks.delete(id); },
      fire(timestamp, dimensions = {}) {
        assert.equal(callbacks.size, 1, 'exactly one outstanding compositor callback');
        const [id, fn] = callbacks.entries().next().value;
        callbacks.delete(id);
        fn(t.now(), { mediaTime: timestamp / 1000000, width: 64, height: 32, ...dimensions });
      },
    };
    videos.push(video);
    return video;
  };
  let video = makeVideo();
  const renderer = createLosslessTrackRenderer({ video: () => video,
    paint: image => paints.push(image.label), send: message => sends.push(message),
    changed: status => statuses.push(status),
    decode: async () => { const bitmap = frame('png'); bitmaps.push(bitmap); if (decodeWait) await decodeWait; return bitmap; },
    ...t,
  });
  const submit = (timestamp, identity = stamp({ sample: '1' }), changes = {}) => {
    const f = Object.assign(frame(`video-${timestamp}`), { timestamp }, changes);
    renderer.submitted(f, identity);
    return f;
  };
  const ready = (identity = stamp({ sample: '1' }), timestamp = 1000001) => {
    renderer.configure({ enabled: true, epoch: identity.epoch });
    renderer.observe(identity); submit(timestamp, identity); video.fire(timestamp); t.run(500);
  };
  return { renderer, t, paints, sends, bitmaps, statuses, videos, submit, ready,
    video: () => video, replaceVideo: () => { video = makeVideo(); return video; },
    defer: () => { let done; decodeWait = new Promise(resolve => { done = resolve; }); return done; },
  };
}

await check('disabled track refinement owns no callbacks, ledger entries, timers, or video copies', async () => {
  const r = trackRig();
  r.renderer.configure({ enabled: false, epoch: 3 });
  for (let i = 1; i <= 100; i++) {
    r.renderer.observe(stamp({ sample: String(i) }));
    const f = r.submit(i, stamp({ sample: String(i) }));
    assert.equal(f.closed, 0); assert.equal(f.clones.length, 0);
  }
  assert.equal(await r.renderer.offer(stamp(), png()), false);
  assert.equal(r.renderer.status().trackedSamples, 0);
  assert.equal(r.renderer.status().callbackPending, false);
  assert.equal(r.video().callbacks.size, 0); assert.equal(r.t.count(), 0);
  assert.deepEqual(r.paints, []); assert.deepEqual(r.sends, []); assert.deepEqual(r.bitmaps, []);
});
await check('track submission cannot request a still until its exact compositor timestamp is presented', () => {
  const r = trackRig(), s = stamp({ sample: '1' });
  r.renderer.configure({ enabled: true, epoch: 3 }); r.renderer.observe(s);
  const f = r.submit(1234567, s);
  r.t.run(500); assert.deepEqual(r.sends, []);
  assert.equal(r.renderer.status().presentedTimestamp, null);
  r.video().fire(1234567); assert.deepEqual(r.sends, []);
  r.t.run(500); assert.equal(r.sends.length, 1);
  assert.equal(r.sends[0].sample, '1'); assert.equal(r.sends[0].op, 'request');
  assert.equal(r.renderer.status().presentedTimestamp, 1234567);
  assert.equal(f.closed, 0); assert.equal(f.clones.length, 0); assert.deepEqual(r.paints, []);
});
await check('track timestamp lookup never guesses a neighboring submission', () => {
  for (const timestamp of [999999, 1000001, NaN, Infinity]) {
    const r = trackRig(), s = stamp({ sample: '1' });
    r.renderer.configure({ enabled: true, epoch: 3 }); r.renderer.observe(s); r.submit(1000000, s);
    r.video().fire(timestamp); r.t.run(500);
    assert.equal(r.renderer.status().shown, false);
    assert.equal(r.renderer.status().presentedTimestamp, null); assert.deepEqual(r.sends, []);
  }
});
await check('an unknown older presentation cannot forget a newer announced and submitted quiet scene', () => {
  const r = trackRig(), s = stamp({ sample: '2', scene: '8' });
  r.renderer.configure({ enabled: true, epoch: 3 });
  r.renderer.observe(s); r.submit(2000000, s);
  r.video().fire(1000000); r.t.run(500); assert.deepEqual(r.sends, []);
  r.video().fire(2000000); r.t.run(500);
  assert.equal(r.sends.filter(m => m.op === 'request').length, 1);
  assert.equal(r.sends.at(-1).scene, '8');
});
await check('track ledger is bounded and an evicted sample never becomes a presented identity', () => {
  const r = trackRig(); r.renderer.configure({ enabled: true, epoch: 3 });
  for (let i = 1; i <= 65; i++) {
    const s = stamp({ sample: String(i) }); r.renderer.observe(s); r.submit(1000000 + i, s);
  }
  assert.equal(r.renderer.status().trackedSamples, 64);
  r.video().fire(1000001); r.t.run(500); assert.deepEqual(r.sends, []);
  r.video().fire(1000065); r.t.run(500);
  assert.equal(r.sends.at(-1).sample, '65'); assert.equal(r.renderer.status().trackedSamples, 0);
});
await check('duplicate or regressing track timestamps remain ambiguous across Off and reset until a newer timestamp recovers', async () => {
  for (const restart of ['none', 'off-on', 'reset']) {
    for (const ambiguousTimestamp of [2000000, 1999999]) {
      const r = trackRig(), first = stamp({ sample: '1' });
      r.renderer.configure({ enabled: true, epoch: 3 }); r.renderer.observe(first); r.submit(2000000, first);
      if (restart === 'off-on') r.renderer.configure({ enabled: false, epoch: 3 });
      if (restart === 'reset') r.renderer.reset();
      if (restart !== 'none') r.renderer.configure({ enabled: true, epoch: 3 });
      const ambiguous = stamp({ sample: '2', scene: '8' });
      r.renderer.observe(ambiguous);
      const f = r.submit(ambiguousTimestamp, ambiguous);
      assert.equal(r.renderer.status().trackedSamples, 0, restart + ' must retire every ambiguous entry');
      r.video().fire(ambiguousTimestamp); r.video().fire(2000000); r.t.run(500);
      assert.equal(r.renderer.status().presentedTimestamp, null);
      assert.equal(await r.renderer.offer(ambiguous, png()), false);
      assert.deepEqual(r.sends, []); assert.deepEqual(r.paints, []);
      assert.equal(f.closed, 0); assert.equal(f.clones.length, 0);
      const newer = stamp({ sample: '3', scene: '9' });
      r.renderer.observe(newer); r.submit(2000001, newer); r.video().fire(2000001); r.t.run(500);
      assert.equal(r.sends.length, 1); assert.equal(r.sends[0].sample, '3');
      assert.equal(await r.renderer.offer(newer, png()), true);
      assert.deepEqual(r.paints, ['png']); assert.equal(r.renderer.status().presentedTimestamp, 2000001);
    }
  }
});
await check('track geometry must agree at submission and compositor presentation', () => {
  for (const dimensions of [{ width: 63 }, { height: 31 }]) {
    const r = trackRig(), s = stamp({ sample: '1' });
    r.renderer.configure({ enabled: true, epoch: 3 }); r.renderer.observe(s); r.submit(1000000, s);
    r.video().fire(1000000, dimensions); r.t.run(500);
    assert.deepEqual(r.sends, []); assert.equal(r.renderer.status().shown, false);
  }
  for (const changes of [{ displayWidth: 63 }, { displayHeight: 31 }, { timestamp: NaN }]) {
    const r = trackRig(); r.renderer.configure({ enabled: true, epoch: 3 });
    r.renderer.observe(stamp({ sample: '1' })); r.submit(1000000, stamp({ sample: '1' }), changes);
    assert.equal(r.renderer.status().trackedSamples, 0);
  }
});
await check('a track PNG never replaces normal video frames or retains a restore copy', async () => {
  const r = trackRig(), s = stamp({ sample: '1' }); r.ready(s);
  assert.equal(await r.renderer.offer(s, png()), true);
  const next = stamp({ sample: '2' }); r.renderer.observe(next);
  const f = r.submit(2000000, next); r.video().fire(2000000);
  assert.deepEqual(r.paints, ['png']); assert.equal(f.clones.length, 0); assert.equal(f.closed, 0);
  assert.equal(r.renderer.status().retainedVideoFrames, 0); assert.equal(r.renderer.status().backupBytes, 0);
  r.renderer.configure({ enabled: false, epoch: 3 });
  assert.equal(r.renderer.status().shown, false); assert.equal(r.bitmaps[0].closed, 1);
  assert.equal(r.renderer.status().trackedSamples, 0); assert.equal(r.video().callbacks.size, 0);
  assert.equal(r.t.count(), 0); assert.deepEqual(r.paints, ['png']);
});
await check('a late track PNG decode after Off closes without painting or requesting again', async () => {
  const r = trackRig(), s = stamp({ sample: '1' }); r.ready(s);
  const done = r.defer(), pending = r.renderer.offer(s, png());
  r.renderer.configure({ enabled: false, epoch: 3 }); done();
  assert.equal(await pending, false); assert.equal(r.bitmaps[0].closed, 1);
  assert.deepEqual(r.paints, []); assert.equal(r.video().callbacks.size, 0); assert.equal(r.t.count(), 0);
  assert.equal(r.sends.filter(m => m.op === 'request').length, 1);
});
await check('a newly received scene retires a track still before its new video is presented', async () => {
  const r = trackRig(), a = stamp({ sample: '1' }); r.ready(a);
  assert.equal(await r.renderer.offer(a, png()), true);
  const b = stamp({ scene: '8', sample: '2' }); r.renderer.observe(b);
  assert.equal(r.renderer.status().shown, false); assert.equal(r.bitmaps[0].closed, 1);
  assert.equal(r.statuses.at(-1).shown, false);
  r.submit(2000000, b); r.t.run(500);
  assert.equal(r.sends.filter(m => m.op === 'request').length, 1);
  r.video().fire(2000000); r.t.run(500);
  assert.equal(await r.renderer.offer(b, png()), true); assert.deepEqual(r.paints, ['png', 'png']);
});
await check('retired track compositor callbacks cannot consume the new session ledger', () => {
  const r = trackRig(), s = stamp({ sample: '1' });
  r.renderer.configure({ enabled: true, epoch: 3 }); r.renderer.observe(s); r.submit(1000000, s);
  const old = r.video().callbacks.values().next().value;
  r.renderer.reset(); r.renderer.configure({ enabled: true, epoch: 3 });
  r.renderer.observe(s); r.submit(2000000, s);
  old(0, { mediaTime: 2, width: 64, height: 32 });
  assert.equal(r.renderer.status().trackedSamples, 1); assert.equal(r.video().callbacks.size, 1);
  r.t.run(500); assert.deepEqual(r.sends, []);
  r.video().fire(2000000); r.t.run(500); assert.equal(r.sends.at(-1).op, 'request');
});
await check('replacing a track video element within one epoch retires its old still and request', async () => {
  for (const visible of [false, true]) {
    const r = trackRig(), s = stamp({ sample: '1' }); r.ready(s);
    if (visible) assert.equal(await r.renderer.offer(s, png()), true);
    const oldVideo = r.video(), oldCallback = oldVideo.callbacks.values().next().value;
    r.replaceVideo(); r.renderer.configure({ enabled: true, epoch: 3 });
    assert.equal(r.renderer.status().shown, false); assert.equal(r.renderer.status().pending, false);
    assert.equal(oldVideo.callbacks.size, 0); assert.equal(r.t.count(), 0);
    if (visible) assert.equal(r.bitmaps[0].closed, 1);
    const next = stamp({ sample: '2' }); r.renderer.observe(next); r.submit(2000000, next);
    oldCallback(0, { mediaTime: 2, width: 64, height: 32 });
    assert.equal(r.renderer.status().trackedSamples, 1);
    r.video().fire(2000000); r.t.run(500);
    assert.equal(r.sends.filter(m => m.op === 'request').length, 2);
  }
});
await check('an old track PNG decode cannot paint over a newer presented scene and queued PNG', async () => {
  const r = trackRig(), a = stamp({ sample: '1' }); r.ready(a);
  const done = r.defer(), old = r.renderer.offer(a, png());
  const b = stamp({ scene: '8', sample: '2' }); r.renderer.observe(b);
  r.submit(2000000, b); r.video().fire(2000000); r.t.run(500);
  assert.equal(await r.renderer.offer(b, png()), false);
  assert.equal(r.renderer.status().queuedPngBytes, 33);
  done(); assert.equal(await old, false);
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(r.bitmaps.length, 2); assert.equal(r.bitmaps[0].closed, 1);
  assert.equal(r.bitmaps[1].closed, 0); assert.deepEqual(r.paints, ['png']);
  assert.equal(r.renderer.status().scene, '8'); assert.equal(r.renderer.status().shown, true);
  assert.equal(r.renderer.status().retainedVideoFrames, 0); assert.equal(r.renderer.status().backupBytes, 0);
});
await check('the actual page track path preserves frame identity and excludes backpressure drops from its ledger', async () => {
  const { readFileSync } = await import('node:fs'), vm = await import('node:vm');
  const source = readFileSync(new URL('../../addons/selkies-web-core/selkies-ws-core.js', import.meta.url), 'utf8');
  const start = source.indexOf('function presentFrameToVideo('), end = source.indexOf('\n}\n', start) + 2;
  assert(start >= 0 && end > start);
  for (const desiredSize of [0, -1, 1]) {
    const writes = [], submitted = [], calls = [];
    const writer = { desiredSize, write: f => { writes.push(f); return Promise.resolve(); } };
    const context = { ensureMstgWriter: () => true, mstgActive: true, mstgRendered: true,
      mstgLastGeom: 'geometry', canvasGeomDirty: false,
      canvas: { style: { display: 'none' }, width: 64, height: 32 },
      videoElement: { style: {} }, canvasContext: { drawImage: () => calls.push('draw') },
      selectLosslessSink: () => {}, syncLosslessVideoStyle: () => {},
      videoFrameWriter: writer, mstgConsecutiveDrops: 0, SINK_STALL_DROP_LIMIT: 30,
      streamStats: { open: false }, trackLossless: { submitted: (f, s) => submitted.push([f, s]) },
      deactivateMstg: () => calls.push('deactivate'), console,
    };
    vm.runInNewContext(source.slice(start, end) + '\nthis.present = presentFrameToVideo;', context);
    const f = Object.assign(frame('ordinary'), { timestamp: 1234567,
      colorSpace: { primaries: 'bt709', transfer: 'bt709', matrix: 'bt709', fullRange: false } });
    const identity = stamp({ sample: '1' }), color = f.colorSpace;
    assert.equal(context.present(f, identity), true);
    assert.equal(f.clones.length, 0); assert.deepEqual(calls, []);
    if (desiredSize <= 0) {
      assert.equal(f.closed, 1); assert.deepEqual(writes, []); assert.deepEqual(submitted, []);
    } else {
      assert.equal(f.closed, 0); assert.equal(writes.length, 1); assert.equal(writes[0], f);
      assert.equal(submitted.length, 1); assert.equal(submitted[0][0], f); assert.equal(submitted[0][1], identity);
      assert.equal(f.timestamp, 1234567); assert.equal(f.colorSpace, color);
      writer.write = () => Promise.reject(Error('writer closed'));
      context.present(frame('rejected'), identity);
      await new Promise(resolve => setImmediate(resolve));
      assert.deepEqual(calls, ['deactivate']);
    }
  }
});
await check('actual capability refresh transfers refinement ownership between page canvas and track within one epoch', async () => {
  const { readFileSync } = await import('node:fs'), vm = await import('node:vm');
  const source = readFileSync(new URL('../../addons/selkies-web-core/selkies-ws-core.js', import.meta.url), 'utf8');
  const extract = name => {
    const start = source.indexOf(`function ${name}(`), end = source.indexOf('\n}\n', start) + 2;
    assert(start >= 0 && end > start);
    return source.slice(start, end);
  };
  const owner = () => ({ enabled: false, epochs: [], configure(config) {
    this.enabled = config.enabled; this.epochs.push(config.epoch);
  } });
  const canvasOwner = owner(), trackOwner = owner();
  const context = { losslessSink: 'page-canvas', lossless_static_refinement: true,
    use_paint_over_quality: true, losslessParentAllowed: true, isVideoPipelineActive: true,
    currentEncoderMode: 'h264enc', isFullFrameVideo: () => true, losslessTrackAvailable: () => true,
    createImageBitmap: () => {}, document: { hidden: false },
    window: { location: { origin: 'https://private-test.invalid' }, postMessage: () => {} },
    losslessServer: { epoch: 3, supported: true, effective: true, reason: 'ready' },
    losslessPresentation: {}, losslessConfiguration: '', losslessConfigSerial: 0, losslessCapabilitySent: '',
    pageLosslessProtocol: { enable: () => {} }, pageLossless: canvasOwner, trackLossless: trackOwner,
    websocket: null, videoWorker: null, sendLossless: () => {}, invalidateLossless: () => {},
  };
  vm.runInNewContext(extract('refreshLosslessCapability') + '\n' + extract('selectLosslessSink')
    + '\nthis.refresh = refreshLosslessCapability; this.select = selectLosslessSink;', context);
  context.refresh();
  assert.deepEqual([canvasOwner.enabled, trackOwner.enabled], [true, false]);
  context.select('track-generator');
  assert.deepEqual([canvasOwner.enabled, trackOwner.enabled], [false, true]);
  context.select('page-canvas');
  assert.deepEqual([canvasOwner.enabled, trackOwner.enabled], [true, false]);
  assert.deepEqual(canvasOwner.epochs, [3, 3, 3]); assert.deepEqual(trackOwner.epochs, [3, 3, 3]);
});

console.log(`[lossless-static] ${passed}/${passed + failed} passed`);
process.exitCode = failed ? 1 : 0;
