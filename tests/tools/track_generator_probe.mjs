/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

/**
 * Runs the video worker's sink handshake against stubs that hold the
 * mediacapture-transform interfaces to their IDL, so the generator path is
 * exercised where no browser the suites can launch exposes it.
 *
 * `VideoTrackGenerator` and `MediaStreamTrackProcessor` are `Exposed=
 * DedicatedWorker`, so a page that asks for either sees `undefined`, and
 * WebKit gates both on `MediaStreamTrackProcessingEnabled`, whose default is
 * on under `PLATFORM(COCOA)` and off in every other build of that engine. The
 * worker source is read out of the client and evaluated here with the globals
 * a dedicated worker has, which
 * is enough to answer what it does with a generator: construct it with no
 * arguments, take the writer from `writable`, and hand `track` to the page in
 * the transfer list, without which the page receives a detached track.
 *
 * The canvas sink an engine without one gets is run the same way, on a clock
 * each draw advances, with frames that count their closes, which no browser
 * reports: a thread that keeps up draws every frame at once, one still busy
 * with its last draw draws only the newest of what arrived, once the work
 * queued ahead of it has run, and every frame is closed exactly once, drawn or
 * not.
 * @module
 */
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';
import { createStripeClock } from '../../addons/selkies-web-core/lib/stripe-clock.js';
import { createPresentMeter } from '../../addons/selkies-web-core/lib/present-meter.js';
import { isSkiaWebKit } from '../../addons/selkies-web-core/lib/util.js';

const TOOLS = dirname(fileURLToPath(import.meta.url));
const WEB = join(TOOLS, '..', '..', 'addons', 'selkies-web-core');
const CORE = join(WEB, 'selkies-ws-core.js');
const WEBCAM = join(WEB, 'lib', 'webcam-capture.js');

let passed = 0, failed = 0;
const check = (label, ok, detail = '') => {
    (ok ? passed++ : failed++);
    console.log(`${ok ? 'PASS' : 'FAIL'}  [track-generator] ${label}  ${detail}`);
};

/** Where `name` is declared in `text`: a `const` or a function, exported or
 *  not, with the offset of its value and of the declaration itself. */
function declarationAt(text, name) {
    const m = new RegExp(`(?:export\\s+)?(?:const|function)\\s+${name}\\s*(=\\s*|\\()`).exec(text);
    if (!m) throw new Error(`${name} not found`);
    const from = m.index + (m[0].startsWith('export ') ? 'export '.length : 0);
    return { from, value: m[1].startsWith('=') ? m.index + m[0].length : from };
}

/** The declaration of `name`, to the brace that closes it, so a helper the
 *  page splices into a worker comes over whole and stays a valid expression. */
function declaration(text, name) {
    const { from } = declarationAt(text, name);
    let depth = 0, seen = false;
    for (let i = from; i < text.length; i++) {
        const c = text[i];
        if (c === '{') { depth++; seen = true; }
        else if (c === '}') { depth--; if (seen && depth === 0) return text.slice(from, i + 1); }
    }
    throw new Error(`${name} is unterminated`);
}

/** The value `name` is declared with, up to the semicolon that ends it. */
function literal(text, name) {
    const { value } = declarationAt(text, name);
    let depth = 0;
    for (let i = value; i < text.length; i++) {
        const c = text[i];
        if ('{[('.includes(c)) depth++;
        else if (')]}'.includes(c)) depth--;
        else if (c === ';' && depth === 0) return text.slice(value, i);
    }
    throw new Error(`${name} is unterminated`);
}

/** Helpers a worker source splices in that its own module imports. */
const IMPORTED = { createStripeClock, createPresentMeter, isSkiaWebKit };

/** Module sources a worker splices in whole, by their `?raw` import name. */
const RAW_SOURCES = {
    wireCodecsSource: () => readFileSync(join(WEB, 'lib', 'wire-codecs.js'), 'utf8').replace(/^export /gm, ''),
    decodeGateSource: () => readFileSync(join(WEB, 'lib', 'decode-gate.js'), 'utf8').replace(/^export /gm, ''),
    decodePaceSource: () => readFileSync(join(WEB, 'lib', 'decode-pace.js'), 'utf8').replace(/^export /gm, ''),
};

/** Resolves one `${...}` the client would have interpolated. */
function splice(text, token) {
    let m = token.match(/^(\w+)\.replace\(\/\^export \/gm, ''\)$/);
    if (m && RAW_SOURCES[m[1]]) return RAW_SOURCES[m[1]]();
    m = token.match(/^(\w+)\.toString\(\)$/);
    if (m) {
        return IMPORTED[m[1]]
            ? IMPORTED[m[1]].toString()
            : declaration(text, m[1]).replace(/^const \S+ = /, '');
    }
    // The encoder candidate table is spliced empty: the track path is what is
    // under test here, and the ladder that reads it has coverage of its own.
    if (/^JSON\.stringify\(\w+\)$/.test(token)) return '[]';
    if (/^\w+$/.test(token)) return literal(text, token);
    throw new Error(`no rule for \${${token}}`);
}

/**
 * The named worker source out of `text`, spliced the way the client splices it
 * before handing it to `new Worker`: every value it interpolates is read back
 * out of the same module, and the template's own escapes are resolved by
 * evaluating it as one.
 */
function workerSource(text, name) {
    const open = text.indexOf(`const ${name} = \``);
    if (open < 0) throw new Error(`${name} not found`);
    const start = text.indexOf('`', open) + 1;
    // The first backtick an even number of backslashes deep closes it; the
    // ones the worker code uses itself are escaped.
    let end = -1;
    for (let i = start; i < text.length; i++) {
        if (text[i] !== '`') continue;
        let slashes = 0;
        while (text[i - 1 - slashes] === '\\') slashes++;
        if (slashes % 2 === 0) { end = i; break; }
    }
    if (end < 0) throw new Error(`${name} is unterminated`);
    let body = text.slice(start, end);
    // Resolved against the module minus this template: the worker source
    // declares the same names it interpolates, and a search over the whole
    // file would answer with the placeholder instead of the value.
    const outside = text.slice(0, open) + text.slice(end);
    const values = [];
    body = body.replace(/(^|[^\\])\$\{([^{}]*(?:\{[^{}]*\}[^{}]*)*)\}/g, (whole, lead, token) => {
        values.push(splice(outside, token));
        return `${lead}__SPLICE${values.length - 1}__`;
    });
    // The module holds this as a template literal, so its `\$` and backtick
    // escapes are the outer template's; evaluating one resolves them exactly.
    body = new Function('return `' + body + '`;')();
    return values.reduce((out, value, i) => out.replace(`__SPLICE${i}__`, () => value), body);
}

/** An IDL-faithful `VideoTrackGenerator`: no constructor arguments, a
 *  `WritableStream` and a `MediaStreamTrack`, and nothing else. */
function generatorStub(state) {
    return class VideoTrackGenerator {
        constructor(...args) {
            if (args.length) throw new TypeError('VideoTrackGenerator takes no arguments');
            state.constructed = true;
            this.writable = new WritableStream({ write: () => {} });
            this.track = { kind: 'video', __mediaStreamTrack: true };
            this.muted = false;
        }
        get readable() { state.readReadable = true; return undefined; }
    };
}

/** Evaluates the worker source with a dedicated worker's globals, over which
 *  `globals` goes; returns what it posted and whether it built a generator. */
function runWorker({ withGenerator, globals = {} }) {
    const state = { constructed: false, readReadable: false, posts: [] };
    const scope = {
        VideoDecoder: class { constructor() {} },
        VideoEncoder: class {},
        VideoFrame: class { constructor() {} close() {} },
        OffscreenCanvas: class { constructor() {} getContext() { return null; } },
        ImageDecoder: class {},
        WritableStream, ReadableStream, MessageChannel,
        createImageBitmap: () => Promise.resolve({}),
        performance, setInterval: () => 0, clearInterval: () => {}, setTimeout, clearTimeout,
        console, navigator: { userAgent: '', platform: '' },
    };
    if (withGenerator) scope.VideoTrackGenerator = generatorStub(state);
    Object.assign(scope, globals);
    const self = {
        postMessage: (msg, transfer) => state.posts.push({ msg, transfer }),
        onmessage: null,
    };
    Object.assign(self, scope);
    scope.self = self;
    vm.createContext(scope);
    new vm.Script(workerSource(readFileSync(CORE, 'utf8'), 'VIDEO_WORKER_SRC'),
                  { filename: 'video-worker.js' }).runInContext(scope);
    state.onmessage = self.onmessage;
    return state;
}

const withGen = runWorker({ withGenerator: true });
const mode = withGen.posts.find((p) => p.msg && p.msg.type === 'mode');
check('a generator is built with no arguments', withGen.constructed, '');
check('the sink it reports is the generator', mode && mode.msg.mode === 'vtg',
      mode && mode.msg.mode);
check('the track goes to the page', !!(mode && mode.msg.track), mode && !!mode.msg.track);
check('and rides the transfer list, so the page gets a live one',
      !!(mode && mode.transfer && mode.transfer.includes(mode.msg.track)),
      mode && mode.transfer);
check('the writer comes from writable, never readable', !withGen.readReadable, '');
check('the striped and jpeg capabilities ride the same reply',
      !!(mode && mode.msg.stripedDecode === true && 'jpegDecode' in mode.msg),
      mode && [mode.msg.stripedDecode, mode.msg.jpegDecode]);
check('the worker is left listening', typeof withGen.onmessage === 'function', '');

const without = runWorker({ withGenerator: false });
const canvasMode = without.posts.find((p) => p.msg && p.msg.type === 'mode');
check('an engine without one is told to send a canvas',
      canvasMode && canvasMode.msg.mode === 'canvas', canvasMode && canvasMode.msg.mode);
check('and nothing is transferred with it',
      canvasMode && !canvasMode.transfer, canvasMode && canvasMode.transfer);

/** A decoded frame that counts its closes. */
const frame = (id) => ({ id, displayWidth: 1280, displayHeight: 800, timestamp: id, closes: 0,
                         close() { this.closes++; } });
/** The worker's clock, which a draw advances by what it costs. */
let clock = 1000;
const DRAW_MS = 10, FRAME_MS = 16.7;
const decoders = [], draws = [];
const sink = runWorker({ withGenerator: false, globals: {
    performance: { now: () => clock, timeOrigin: 0 },
    VideoDecoder: class {
        constructor(init) { this.init = init; this.state = 'unconfigured'; this.decodeQueueSize = 0; decoders.push(this); }
        configure() { this.state = 'configured'; }
        decode() {}
        close() { this.state = 'closed'; }
        static isConfigSupported() { return Promise.resolve({ supported: true }); }
    },
} });
const tell = (data) => sink.onmessage({ data });
tell({ canvas: { width: 300, height: 150, getContext: () => ({
    // A browser refuses to draw a closed frame.
    drawImage: (f) => {
        if (f.closes) throw new Error(`frame ${f.id} drawn after its close`);
        draws.push(f.id);
        clock += DRAW_MS;
    },
}) } });
tell({ type: 'statsOpen', open: true });
tell({ type: 'decoderConfig', codec: 'avc1.42e01f', codedWidth: 1280, codedHeight: 800 });
const decoded = (f) => decoders[decoders.length - 1].init.output(f);
const nextTask = () => new Promise((resolve) => setTimeout(resolve, 5));

const kept = [frame(1), frame(2)];
decoded(kept[0]);
clock += FRAME_MS;
decoded(kept[1]);
check('a canvas that keeps up draws each decoded frame at once', JSON.stringify(draws) === '[1,2]', draws);
const burst = [frame(3), frame(4), frame(5)];
burst.forEach(decoded);
check('one decoded while its last draw is still recent waits for the work queued ahead of it',
      JSON.stringify(draws) === '[1,2]', draws);
await nextTask();
check('then only the newest of a burst is drawn', JSON.stringify(draws) === '[1,2,5]', draws);
clock += FRAME_MS;
const paced = frame(6);
decoded(paced);
check('and a thread caught up again draws at once', JSON.stringify(draws) === '[1,2,5,6]', draws);
const stale = frame(7);
decoded(stale);
tell({ type: 'wireMode', striped: true });
await nextTask();
check('a frame its decoder left behind is dropped, never drawn over what replaced it',
      !draws.includes(7), draws);
tell({ type: 'decodeStats' });
const stats = sink.posts.filter((p) => p.msg && p.msg.type === 'decodeStats').pop();
check('the frames it never drew count as not shown',
      stats && stats.msg.shown.presented === 4 && stats.msg.shown.superseded === 3,
      stats && JSON.stringify(stats.msg.shown));
const all = [...kept, ...burst, paced, stale];
check('every frame is closed exactly once, drawn or not', all.every((f) => f.closes === 1), all.map((f) => f.closes));
check('and the page hears once that the canvas holds a picture',
      sink.posts.filter((p) => p.msg && p.msg.type === 'presented').length === 1,
      sink.posts.map((p) => p.msg && p.msg.type).join());

/** An IDL-faithful `MediaStreamTrackProcessor`: a dictionary carrying a video
 *  track, and a `ReadableStream` of frames. */
function processorStub(state) {
    return class MediaStreamTrackProcessor {
        constructor(init) {
            if (!init || typeof init !== 'object' || !init.track) {
                throw new TypeError('MediaStreamTrackProcessor takes { track }');
            }
            state.processorTrack = init.track;
            this.readable = new ReadableStream({ pull() { /* never resolves */ } });
        }
    };
}

/** Runs the webcam's encode worker and hands it a transferred camera track. */
function runEncodeWorker({ withProcessor }) {
    const state = { posts: [], processorTrack: null };
    const scope = {
        VideoEncoder: class { constructor() {} static isConfigSupported() { return Promise.resolve({ supported: false }); } },
        VideoFrame: class { constructor() {} close() {} },
        OffscreenCanvas: class { constructor() {} getContext() { return null; } },
        ImageEncoder: class {}, ReadableStream, WritableStream,
        createImageBitmap: () => Promise.resolve({}),
        performance, setTimeout, clearTimeout, setInterval: () => 0, clearInterval: () => {},
        console,
    };
    if (withProcessor) scope.MediaStreamTrackProcessor = processorStub(state);
    const self = { postMessage: (msg) => state.posts.push(msg), onmessage: null };
    Object.assign(self, scope);
    scope.self = self;
    vm.createContext(scope);
    new vm.Script(workerSource(readFileSync(WEBCAM, 'utf8'), 'ENCODE_WORKER_SRC'),
                  { filename: 'encode-worker.js' }).runInContext(scope);
    const track = { kind: 'video', __mediaStreamTrack: true };
    self.onmessage({ data: { type: 'track', track } });
    state.track = track;
    return state;
}

const camera = runEncodeWorker({ withProcessor: true });
check('the camera track is read through a processor', camera.processorTrack === camera.track,
      camera.processorTrack === camera.track);
check('which is told so in a dictionary, as its init takes',
      camera.posts.some((m) => m && m.type === 'track_reading'),
      camera.posts.map((m) => m && m.type));

const noProcessor = runEncodeWorker({ withProcessor: false });
check('a worker without one says so instead of throwing',
      noProcessor.posts.some((m) => m && m.type === 'track_unsupported'),
      noProcessor.posts.map((m) => m && m.type));

console.log(`[track-generator] ${passed}/${passed + failed} passed`);
process.exit(failed ? 1 : 0);
